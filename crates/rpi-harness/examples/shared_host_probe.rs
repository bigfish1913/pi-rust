//! Synthetic shared-host baseline. Drive with scripts/bench-shared-host.py.
//! Only fresh fixture directories and an explicit loopback HTTP provider are allowed.
//! This is NOT a production host or a proof of a 1 GB full-workload bound.
use std::{
    io::{BufRead, Write},
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use async_trait::async_trait;
use rpi_agent::{AgentError, AgentTool, AgentToolResult, ToolResultPartial};
use rpi_ai::providers::openai_completions::OpenAiCompletionsProvider;
use rpi_ai::{
    types::{Context as ProviderContext, Message},
    Api, Model, Provider, Tool,
};
use rpi_harness::{
    agent_harness::{AgentHarness, AgentLane, HarnessRunOutcome},
    session::{
        jsonl::{JsonlSessionCreateOptions, JsonlSessionRepo, JsonlSessionRepoOptions},
        memory::SystemClock,
        session::DefaultIdGenerator,
        Session,
    },
    types::{AgentHarnessOptions, HarnessTool},
};
use rpi_tools::{ExecutionToolContext, OsExecutionEnv};
use serde_json::{json, Value};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{mpsc, Semaphore},
    task::JoinSet,
};
use tokio_util::sync::CancellationToken;

type Error = Box<dyn std::error::Error + Send + Sync>;

fn emit(value: Value) {
    let mut out = std::io::stdout().lock();
    writeln!(out, "{value}").expect("probe stdout closed");
    out.flush().expect("probe stdout flush failed");
}

struct Options {
    root: PathBuf,
    agents: usize,
    mode: String,
    base_url: String,
    context_bytes: usize,
    history_entries: usize,
    tool_kind: String,
    workers: usize,
    max_blocking_threads: usize,
    stagger_ms: u64,
    timeout: u64,
    cgroup: Option<PathBuf>,
}
impl Options {
    fn parse() -> Result<Self, Error> {
        let mut o = Self {
            root: PathBuf::new(),
            agents: 8,
            mode: "tool".into(),
            base_url: String::new(),
            context_bytes: 1024,
            history_entries: 0,
            tool_kind: "async".into(),
            workers: 4,
            max_blocking_threads: 512,
            stagger_ms: 0,
            timeout: 120,
            cgroup: None,
        };
        let mut args = std::env::args().skip(1);
        while let Some(key) = args.next() {
            let value = args.next().ok_or("expected --option value pairs")?;
            match key.as_str() {
                "--output-root" => o.root = value.into(),
                "--agents" => o.agents = value.parse()?,
                "--mode" => o.mode = value,
                "--base-url" => o.base_url = value,
                "--context-bytes" => o.context_bytes = value.parse()?,
                "--history-entries" => o.history_entries = value.parse()?,
                "--tool-kind" => o.tool_kind = value,
                "--workers" => o.workers = value.parse()?,
                "--max-blocking-threads" => o.max_blocking_threads = value.parse()?,
                "--stagger-ms" => o.stagger_ms = value.parse()?,
                "--timeout" => o.timeout = value.parse()?,
                "--cgroup" => o.cgroup = Some(value.into()),
                _ => return Err(format!("unknown option {key}").into()),
            }
        }
        if o.root.as_os_str().is_empty()
            || o.agents == 0
            || o.agents > 10_000
            || o.workers == 0
            || o.max_blocking_threads == 0
            || o.stagger_ms > 60_000
            || o.timeout == 0
            || !["empty", "idle", "model", "raw", "tool"].contains(&o.mode.as_str())
            || !["async", "bash"].contains(&o.tool_kind.as_str())
        {
            return Err(
                "invalid root, count, mode, tool kind, workers, blocking pool or timeout".into(),
            );
        }
        let url = reqwest::Url::parse(&o.base_url)?;
        let loopback = url
            .host_str()
            .and_then(|h| h.parse::<std::net::IpAddr>().ok())
            .is_some_and(|ip| ip.is_loopback());
        if url.scheme() != "http"
            || !loopback
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(
                "base URL must be explicit loopback HTTP without credentials/query/fragment".into(),
            );
        }
        Ok(o)
    }
}

/// Arrivals count only executions INSIDE the tool, after bash printed readiness.
struct ToolGate {
    expected: usize,
    entered: AtomicUsize,
    live: AtomicUsize,
    peak: AtomicUsize,
    permits: Semaphore,
}
struct LiveTool(Arc<ToolGate>);
impl Drop for LiveTool {
    fn drop(&mut self) {
        self.0.live.fetch_sub(1, Ordering::SeqCst);
    }
}
impl ToolGate {
    fn new(expected: usize) -> Self {
        Self {
            expected,
            entered: AtomicUsize::new(0),
            live: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            permits: Semaphore::new(0),
        }
    }
    async fn wait(self: &Arc<Self>, signal: &CancellationToken) -> Result<(), AgentError> {
        let _live = LiveTool(self.clone());
        let live = self.live.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(live, Ordering::SeqCst);
        let entered = self.entered.fetch_add(1, Ordering::SeqCst) + 1;
        if entered == self.expected {
            emit(json!({"phase":"tools_held", "entered":entered, "live":live}));
        }
        tokio::select! {
            _ = signal.cancelled() => Err(AgentError::Tool("probe cancelled".into())),
            permit = self.permits.acquire() => {
                permit.map_err(|_| AgentError::Tool("gate closed".into()))?.forget();
                Ok(())
            }
        }
    }
}

struct ProbeTool {
    schema: Tool,
    gate: Arc<ToolGate>,
    agent: usize,
    bash: bool,
    cwd: PathBuf,
}
#[async_trait]
impl AgentTool for ProbeTool {
    fn schema(&self) -> &Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "synthetic concurrency barrier"
    }
    async fn execute(
        &self,
        _: &str,
        params: Value,
        signal: CancellationToken,
        _: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        if params.get("agent").and_then(Value::as_u64) != Some(self.agent as u64) {
            return Err(AgentError::Tool("wrong agent tool binding".into()));
        }
        if self.bash {
            let mut child = tokio::process::Command::new("bash")
                .args([
                    "--noprofile",
                    "--norc",
                    "-c",
                    "printf 'ready\\n'; IFS= read -r release || exit 2; printf 'ok\\n'",
                ])
                .env_clear()
                .current_dir(&self.cwd)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true)
                .spawn()
                .map_err(|e| AgentError::Tool(format!("bash spawn: {e}")))?;
            let mut stdout = child.stdout.take().expect("piped stdout");
            let mut ready = [0; 6];
            tokio::select! {
                _ = signal.cancelled() => return Err(AgentError::Tool("probe cancelled".into())),
                result = stdout.read_exact(&mut ready) => { result.map_err(|e| AgentError::Tool(e.to_string()))?; }
            }
            if &ready != b"ready\n" {
                return Err(AgentError::Tool("bash readiness missing".into()));
            }
            self.gate.wait(&signal).await?;
            child
                .stdin
                .take()
                .expect("piped stdin")
                .write_all(b"release\n")
                .await
                .map_err(|e| AgentError::Tool(e.to_string()))?;
            let status = child
                .wait()
                .await
                .map_err(|e| AgentError::Tool(e.to_string()))?;
            if !status.success() {
                return Err(AgentError::Tool(format!("bash: {status}")));
            }
        } else {
            self.gate.wait(&signal).await?;
        }
        Ok(AgentToolResult::text(format!(
            "tool completed {}",
            self.agent
        )))
    }
}

async fn command(input: &mut mpsc::Receiver<String>, expected: &str) -> Result<(), Error> {
    match input.recv().await.as_deref() {
        Some(actual) if actual == expected => Ok(()),
        other => Err(format!("expected controller command {expected}, got {other:?}").into()),
    }
}

async fn probe(o: &Options, input: &mut mpsc::Receiver<String>) -> Result<(), Error> {
    // Never reuse user sessions, a prior fixture, or an existing output directory.
    std::fs::create_dir(&o.root)?;
    let root = std::fs::canonicalize(&o.root)?;
    std::fs::write(root.join("PROBE_ONLY"), "synthetic-shared-host-v1\n")?;
    let env = Arc::new(OsExecutionEnv::with_cwd(root.clone()));
    let repo = JsonlSessionRepo::with_env_cwd(JsonlSessionRepoOptions {
        fs: env.clone(),
        sessions_root: root.join("sessions").to_string_lossy().into_owned(),
        clock: Arc::new(SystemClock),
        ids: Arc::new(DefaultIdGenerator::new()),
    });
    let model = Model::new(
        "probe",
        "Synthetic probe",
        Api::OpenaiCompletions,
        "shared-host-probe",
        o.base_url.clone(),
    );
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let provider = Arc::new(OpenAiCompletionsProvider::with_models_without_auth(
        "shared-host-probe",
        client,
        vec![model.clone()],
    ));
    let gate = Arc::new(ToolGate::new(o.agents));
    let schema = Tool {
        name: "probe_barrier".into(),
        description: "Synthetic concurrency fixture only".into(),
        parameters: rpi_ai::Schema::new(
            json!({"type":"object","properties":{"agent":{"type":"integer"}},
            "required":["agent"],"additionalProperties":false}),
        ),
        constrained_sampling: None,
    };
    let ctx = ExecutionToolContext::new(env.clone(), Some(env.clone()));
    let builtin = vec![
        HarnessTool::new(rpi_tools::create_read_tool(&ctx, None)),
        HarnessTool::new(rpi_tools::create_write_tool(&ctx)),
        HarnessTool::new(rpi_tools::create_edit_tool(&ctx)),
        HarnessTool::new(rpi_tools::create_bash_tool(&ctx, None)),
    ];
    let history_body = "h".repeat(o.context_bytes);
    let mut harnesses = Vec::with_capacity(o.agents);
    let started = Instant::now();
    let mut session_time = Duration::ZERO;
    let mut harness_time = Duration::ZERO;
    let mut history_time = Duration::ZERO;
    for agent in 0..o.agents {
        // `raw` measures the provider path alone: no Session, Harness or Agent.
        if o.mode == "raw" {
            break;
        }
        let stage = Instant::now();
        let storage = repo
            .create_typed(&JsonlSessionCreateOptions {
                id: Some(format!("probe-agent-{agent}")),
                parent_session_id: None,
                cwd: root.to_string_lossy().into_owned(),
                metadata: None,
            })
            .await?;
        storage.acquire_writer_lease().await?;
        session_time += stage.elapsed();
        let stage = Instant::now();
        let session = Session::new(Arc::new(storage), None);
        let mut tools = if o.mode == "empty" {
            Vec::new()
        } else {
            builtin.clone()
        };
        if o.mode != "empty" {
            tools.push(HarnessTool::new(Arc::new(ProbeTool {
                schema: schema.clone(),
                gate: gate.clone(),
                agent,
                bash: o.tool_kind == "bash",
                cwd: root.clone(),
            })));
        }
        let harness = AgentHarness::create(AgentHarnessOptions {
            model: model.clone(),
            session,
            tools,
            models: if o.mode == "empty" {
                Vec::new()
            } else {
                vec![provider.clone()]
            },
            system_prompt: (o.mode != "empty")
                .then(|| "Synthetic shared-host benchmark. Use only the scripted tool.".into()),
            require_native_durable: true,
            ..Default::default()
        })
        .await?;
        harness_time += stage.elapsed();
        let stage = Instant::now();
        for _ in 0..o.history_entries {
            harness
                .append_message(rpi_ai::UserMessage::new(history_body.clone(), 1).into())
                .await?;
        }
        history_time += stage.elapsed();
        harnesses.push(harness);
        if (agent + 1) % 100 == 0 {
            emit(json!({"phase":"setup_progress", "agentsCreated":agent + 1,
                "setupMs":started.elapsed().as_millis(),
                "sessionCreateMs":session_time.as_millis(), "harnessCreateMs":harness_time.as_millis(),
                "historyAppendMs":history_time.as_millis()}));
        }
    }
    emit(json!({"phase":"ready", "agents":o.agents, "mode":o.mode,
        "toolKind":o.tool_kind, "contextBytes":o.context_bytes, "historyEntries":o.history_entries,
        "workers":o.workers, "maxBlockingThreads":o.max_blocking_threads, "staggerMs":o.stagger_ms,
        "setupMs":started.elapsed().as_millis(), "sessions":if o.mode == "raw" { "none" } else { "independent-jsonl" },
        "sessionCreateMs":session_time.as_millis(), "harnessCreateMs":harness_time.as_millis(),
        "historyAppendMs":history_time.as_millis(),
        "extensions":false, "tls":false}));
    command(input, "run").await?;
    let started = Instant::now();
    let mut completed = 0;
    if o.mode == "raw" {
        // Attribute provider/HTTP cost with no Session, Harness or Agent loop.
        let mut tasks = JoinSet::new();
        let stagger_ms = o.stagger_ms;
        for agent in 0..o.agents {
            let provider = provider.clone();
            let model = model.clone();
            let prompt = format!("probe-agent:{agent}\n{}", "x".repeat(o.context_bytes));
            tasks.spawn(async move {
                // Fixture constraint: macOS clamps listen() to kern.ipc.somaxconn
                // (128 by default) and RESETs overflowing connection bursts, so
                // optional staggering spaces connection establishment. The
                // barrier still requires every request body to be complete and
                // held before any response.
                if stagger_ms > 0 {
                    tokio::time::sleep(Duration::from_millis(stagger_ms * agent as u64)).await;
                }
                let options = rpi_ai::provider::SimpleStreamOptions::default();
                let context =
                    ProviderContext::new(vec![Message::User(rpi_ai::UserMessage::new(prompt, 1))]);
                let message = provider
                    .stream_simple(&model, &context, &options)
                    .await
                    .result()
                    .await?;
                let text = rpi_ai::Content::text_only(&message.content, "");
                if text != format!("completed {agent}") {
                    return Err(format!(
                        "agent {agent}: unexpected final text {text:?} (stop {:?}: {:?})",
                        message.stop_reason, message.error_message
                    )
                    .into());
                }
                Ok::<_, Error>(())
            });
        }
        while let Some(result) = tasks.join_next().await {
            result??;
            completed += 1;
        }
    }
    if o.mode == "model" || o.mode == "tool" {
        let mut tasks = JoinSet::new();
        for (agent, harness) in harnesses.iter().enumerate() {
            let harness = harness.clone();
            let prompt = format!("probe-agent:{agent}\n{}", "x".repeat(o.context_bytes));
            tasks.spawn(async move {
                let result = harness.prompt_text(&prompt, vec![]).await?;
                match result.outcome {
                    HarnessRunOutcome::Completed { final_message, .. } => {
                        let text: String = final_message
                            .content
                            .iter()
                            .filter_map(|c| match c {
                                rpi_ai::Content::Text(t) => Some(t.text.as_str()),
                                _ => None,
                            })
                            .collect();
                        if text != format!("completed {agent}") {
                            return Err(
                                format!("agent {agent}: unexpected final text {text:?}").into()
                            );
                        }
                    }
                    other => return Err(format!("agent {agent}: {other:?}").into()),
                }
                if !harness.take_write_failures().is_empty() {
                    return Err("session persistence failed".into());
                }
                Ok::<_, Error>(())
            });
        }
        let mut released = false;
        while !tasks.is_empty() {
            tokio::select! {
                result = tasks.join_next() => {
                    result.ok_or("missing run task")???;
                    completed += 1;
                }
                cmd = input.recv() => {
                    if cmd.as_deref() != Some("release_tools") || released || o.mode != "tool"
                        || gate.entered.load(Ordering::SeqCst) != o.agents
                        || gate.live.load(Ordering::SeqCst) != o.agents
                    {
                        return Err(format!("unexpected controller command {cmd:?}").into());
                    }
                    released = true;
                    gate.permits.add_permits(o.agents);
                }
            }
        }
        if o.mode == "tool"
            && (!released
                || gate.peak.load(Ordering::SeqCst) != o.agents
                || gate.entered.load(Ordering::SeqCst) != o.agents)
        {
            return Err("tool concurrency was not proven".into());
        }
    }
    for harness in &harnesses {
        harness.drain_session().await;
        if !harness.take_write_failures().is_empty() {
            return Err("session persistence failed during drain".into());
        }
    }
    emit(
        json!({"phase":"settled", "agents":if o.mode == "raw" { o.agents } else { harnesses.len() }, "completed":completed,
        "toolEntered":gate.entered.load(Ordering::SeqCst), "toolPeak":gate.peak.load(Ordering::SeqCst),
        "runMs":started.elapsed().as_millis()}),
    );
    // Keep all Harness/Session/provider bindings alive while the controller samples.
    command(input, "finish").await?;
    for harness in &harnesses {
        harness.close().await?;
    }
    let closed = harnesses.len();
    drop(harnesses);
    // A sample taken after this phase separates live per-Agent retention from
    // allocator pages that the process has not returned to the OS.
    emit(json!({"phase":"closed", "agents":closed,
        "runMs":started.elapsed().as_millis()}));
    command(input, "closed_sampled").await?;
    emit(json!({"phase":"complete", "success":true}));
    Ok(())
}

fn main() {
    let result = (|| -> Result<(), Error> {
        let o = Options::parse()?;
        if let Some(cgroup) = &o.cgroup {
            if !cfg!(target_os = "linux") {
                return Err("cgroup enrollment is Linux-only".into());
            }
            if !std::fs::read_to_string(cgroup.join("cgroup.procs"))?
                .trim()
                .is_empty()
            {
                return Err("measurement cgroup must be empty before enrollment".into());
            }
            // Enroll before runtime, tools or fixtures allocate; children inherit it.
            std::fs::write(cgroup.join("cgroup.procs"), std::process::id().to_string())?;
        }
        let (tx, mut input) = mpsc::channel(4);
        std::thread::spawn(move || {
            for line in std::io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                if tx.blocking_send(line).is_err() {
                    break;
                }
            }
        });
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(o.workers)
            // Budget blocking I/O, not Agent/model concurrency. Every Agent must
            // still reach the HTTP and tool barriers before either is released.
            .max_blocking_threads(o.max_blocking_threads)
            .enable_all()
            .build()?;
        let result = runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(o.timeout), probe(&o, &mut input))
                .await
                .map_err(|_| -> Error { "probe deadline exceeded".into() })?
        });
        runtime.shutdown_timeout(Duration::from_secs(2));
        result
    })();
    if let Err(error) = result {
        emit(json!({"phase":"error", "success":false,"error":error.to_string()}));
        std::process::exit(1);
    }
}
