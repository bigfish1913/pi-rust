//! Harness construction + session-storage wiring. Mirrors the Rust-side
//! equivalent of the TS `packages/coding-agent/src/core/sdk.ts`
//! (`createAgentSession`) — build the env, tools, durable session storage, and
//! `AgentHarnessOptions`, then `AgentHarness::create`.
//!
//! v1 scope cuts vs the TS SDK (tracked in `docs/m6-cli-open-questions.md`):
//! - **No extension / skill / prompt-template / theme / context-file discovery.**
//!   The harness `resources` stay empty; the system prompt is either the
//!   caller's `--system-prompt` or the built-in default ([`default_system_prompt`]).
//! - **No `--models` cycling, no `ModelRuntime`/multi-provider.** One model,
//!   one provider (Anthropic), resolved up-front by [`crate::provider`].
//! - **Built-in tools**: `read`, `bash`, `edit`, `write` plus the read-only
//!   `grep`/`find`/`ls` (the TS `createCodingTools` default set). `grep`/`find`
//!   use an in-process `FileSystem`+`regex`/`globset` implementation (documented
//!   divergence from the TS `rg`/`fd` shell-out; see `docs/m4-tools-open-questions.md`).
//! - **Session restore (`-c`/`-r`/`--session`)** is *partially* supported: a
//!   fresh session is always created. The harness's `create` rejects sessions
//!   that already have records (restore not implemented — M5f divergence #3),
//!   so `-c`/`-r`/`--session` currently surface a clear "not implemented"
//!   message rather than silently starting fresh. See [`SessionSelection`].

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rpi_ai::Provider;
use rpi_harness::agent_harness::AgentHarness;
use rpi_harness::session::memory::{InMemorySessionStorage, SystemClock};
use rpi_harness::session::session::DefaultIdGenerator;
use rpi_harness::session::types::SessionMetadata;
use rpi_harness::session::Session;
use rpi_harness::types::{
    AgentHarnessOptions, AgentHarnessResources, CompactionSettings, DrivingMode,
    HarnessToolExecution, HarnessTool, RetryPolicy, ToolReplay,
};
use rpi_tools::{
    create_bash_tool, create_edit_tool, create_find_tool, create_grep_tool, create_ls_tool,
    create_read_tool, create_write_tool, ExecutionToolContext, MutationQueueRegistry,
    OsExecutionEnv,
};

use crate::args::Args;
use crate::provider::ResolvedModel;

/// The built-in tool names v1 ships, in the order the TS `createCodingTools`
/// registers them: the mutating set (`read`/`bash`/`edit`/`write`) followed by
/// the read-only search set (`grep`/`find`/`ls`).
pub const BUILTIN_TOOL_NAMES: &[&str] = &["read", "bash", "edit", "write", "grep", "find", "ls"];

/// The default coding system prompt. A condensed port of the TS
/// `packages/coding-agent/src/core/system-prompt.ts` base prompt — the
/// pi-internal docs/skills/context-file sections are omitted (v1 has none of
/// that machinery), leaving the role + tools + guidelines core.
pub fn default_system_prompt(cwd: &str) -> String {
    format!(
        "You are an expert coding assistant operating inside pi, a coding agent harness. \
You help users by reading files, executing commands, editing code, and writing new files.

Available tools:
- read  — Read file contents
- bash  — Execute shell commands
- edit  — Find/replace edits to existing files
- write — Create or overwrite files
- grep  — Search file contents for a pattern
- find  — Search for files by glob pattern
- ls    — List directory contents

Guidelines:
- Be concise in your responses
- Show file paths clearly when working with files
- Prefer the smallest change that solves the problem

Current working directory: {cwd}"
    )
}

/// How the user asked to select a session. v1 only honors `NoSession`
/// (ephemeral `InMemorySessionStorage`) and `New` (a fresh JSONL file). The
/// continue/resume/specific-session paths are recognized but not wired (the
/// harness rejects restore — see module docs).
#[derive(Debug, Clone)]
pub enum SessionSelection {
    /// `--no-session`: ephemeral, in-memory, nothing persisted.
    Ephemeral,
    /// Fresh durable JSONL session under `--session-dir` (or the default dir).
    New { dir: PathBuf, name: Option<String> },
    /// `-c` / `-r` / `--session <id|path>`: requested an existing session.
    /// v1 can't restore it, so [`build`] surfaces an error.
    Existing { requested: String },
}

/// Decide the session selection from parsed args + the resolved cwd.
pub fn select_session(args: &Args, cwd: &Path) -> SessionSelection {
    if args.no_session {
        return SessionSelection::Ephemeral;
    }
    if args.continue_session {
        return SessionSelection::Existing { requested: "--continue".into() };
    }
    if args.resume {
        return SessionSelection::Existing { requested: "--resume".into() };
    }
    if let Some(s) = &args.session {
        return SessionSelection::Existing { requested: s.clone() };
    }
    let dir = args
        .session_dir
        .clone()
        .unwrap_or_else(|| default_session_dir(cwd));
    SessionSelection::New { dir, name: args.name.clone() }
}

/// The default session directory: `<cwd>/.pi/sessions`. Mirrors the TS
/// `getDefaultSessionDir` (`.pi/agent/sessions` in TS; v1 uses `.pi/sessions`
/// under the project — a documented divergence).
pub fn default_session_dir(cwd: &Path) -> PathBuf {
    cwd.join(".pi").join("sessions")
}

/// Build the `AgentHarness` from the resolved model + parsed args + cwd.
///
/// This is the v1 equivalent of TS `createAgentSession`. It:
/// 1. Builds the `OsExecutionEnv` rooted at `cwd`.
/// 2. Constructs the built-in tools (optionally filtered by `--tools`/
///    `--exclude-tools`/`--no-tools`/`--no-builtin-tools`).
/// 3. Resolves the session storage (ephemeral vs fresh JSONL vs restore-error).
/// 4. Assembles `AgentHarnessOptions` and calls `AgentHarness::create`.
pub async fn build(
    resolved: &ResolvedModel,
    args: &Args,
    cwd: &Path,
) -> Result<AgentHarness, BuildError> {
    let cwd_str = cwd.to_string_lossy().to_string();

    // ---- Execution env + tools ----
    let env = Arc::new(OsExecutionEnv::with_cwd(cwd.to_path_buf()));
    let env_dyn: Arc<dyn rpi_tools::ExecutionEnv> = env.clone();
    let mut_env: Arc<dyn rpi_tools::MutatingEnv> = env.clone();
    let _registry = Arc::new(MutationQueueRegistry::new());
    let ctx = ExecutionToolContext::new(env_dyn, Some(mut_env));

    let tools = build_tools(&ctx, args);
    let active = active_tool_names(&tools, args);

    // ---- Session storage ----
    let selection = select_session(args, cwd);
    let session = build_session(&selection, &cwd_str).await?;

    // ---- System prompt ----
    let base_prompt = args
        .system_prompt
        .clone()
        .unwrap_or_else(|| default_system_prompt(&cwd_str));
    let system_prompt = if args.append_system_prompt.is_empty() {
        base_prompt
    } else {
        // Append each `--append-system-prompt` (text or, if it's a readable
        // file path, the file contents — mirrors the TS behavior where the
        // flag accepts either).
        let mut out = base_prompt;
        for extra in &args.append_system_prompt {
            let text = read_append_target(extra).unwrap_or_else(|| extra.clone());
            out.push_str("\n\n");
            out.push_str(&text);
        }
        out
    };

    // ---- Options ----
    let options = AgentHarnessOptions {
        model: resolved.model.clone(),
        thinking_level: resolved.thinking_level,
        active_tool_names: active,
        tools,
        system_prompt: Some(system_prompt),
        resources: AgentHarnessResources::empty(),
        stream_options: Default::default(),
        retry: RetryPolicy::default(),
        compaction: CompactionSettings::default(),
        steering_mode: Default::default(),
        follow_up_mode: Default::default(),
        tool_execution: HarnessToolExecution::default(),
        drive: DrivingMode::default(),
        session,
        models: vec![resolved.provider.clone() as Arc<dyn Provider>],
        to_provider_messages: None,
        entry_projectors: Default::default(),
    };

    AgentHarness::create(options)
        .await
        .map_err(|e| BuildError::HarnessCreate(e.to_string()))
}

/// A harness-build error.
#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("Could not create the session directory: {0}")]
    SessionDir(String),
    #[error("Session restore is not implemented in v1 (requested: {requested}). Start a fresh session instead (drop {flag}).")]
    RestoreNotImplemented { requested: String, flag: &'static str },
    #[error("Could not build the harness: {0}")]
    HarnessCreate(String),
}

/// Build the tool list per `--tools`/`--exclude-tools`/`--no-tools`/
/// `--no-builtin-tools`. Mirrors the TS `tools`/`excludeTools`/`noTools`
/// resolution in `createAgentSession`.
fn build_tools(ctx: &ExecutionToolContext, args: &Args) -> Vec<HarnessTool> {
    if args.no_tools {
        return Vec::new();
    }
    // Construct every built-in once (cheap; the allowlist filters below).
    // Read-only search tools (grep/find/ls) take the same context and need no
    // mutation queue — they go through the `FileSystem` trait only.
    let mut all: Vec<(&'static str, HarnessTool)> = vec![
        ("read", HarnessTool::new(create_read_tool(ctx, None))),
        ("bash", HarnessTool::new(create_bash_tool(ctx, None))),
        ("edit", HarnessTool::new(create_edit_tool(ctx))),
        ("write", HarnessTool::new(create_write_tool(ctx))),
        ("grep", HarnessTool::new(create_grep_tool(ctx, None))),
        ("find", HarnessTool::new(create_find_tool(ctx, None))),
        ("ls", HarnessTool::new(create_ls_tool(ctx, None))),
    ];

    // `--no-builtin-tools` disables the built-in set but would keep
    // extension/custom tools — v1 has none, so it's equivalent to `--no-tools`
    // here. We honor it by clearing the built-ins.
    if args.no_builtin_tools {
        all.clear();
    }

    // Allowlist (`--tools`): keep only named built-ins.
    if let Some(allow) = &args.tools {
        all.retain(|(name, _)| allow.iter().any(|a| a == name));
    }
    // Denylist (`--exclude-tools`): drop named tools.
    if let Some(deny) = &args.exclude_tools {
        all.retain(|(name, _)| !deny.iter().any(|d| d == name));
    }

    all.into_iter().map(|(_, t)| t.with_replay(ToolReplay::Safe)).collect()
}

/// Resolve the active tool names from the constructed tools when no explicit
/// `--tools` allowlist was given. Mirrors the TS default: all registered tools
/// active.
fn active_tool_names(tools: &[HarnessTool], args: &Args) -> Vec<String> {
    if args.no_tools {
        return Vec::new();
    }
    if let Some(allow) = &args.tools {
        // The allowlist IS the active set (TS: `tools` doubles as the active
        // set when provided). Keep order + only those that exist.
        let names: Vec<String> = tools.iter().map(|t| t.tool.schema().name.clone()).collect();
        return allow.iter().filter(|a| names.iter().any(|n| n == *a)).cloned().collect();
    }
    // Default: every constructed tool is active. If `--exclude-tools` dropped
    // some, they're simply absent from `tools`, so this lands right.
    tools.iter().map(|t| t.tool.schema().name.clone()).collect()
}

/// Build the `Session` facade for the chosen selection.
async fn build_session(selection: &SessionSelection, cwd: &str) -> Result<Session, BuildError> {
    match selection {
        SessionSelection::Ephemeral => Ok(ephemeral_session()),
        SessionSelection::New { dir, .. } => {
            // Ensure the sessions directory exists, then create a fresh JSONL
            // session file inside it.
            std::fs::create_dir_all(dir)
                .map_err(|e| BuildError::SessionDir(format!("{}: {e}", dir.display())))?;
            let session = create_jsonl_session(dir, cwd)
                .await
                .map_err(|e| BuildError::SessionDir(format!("{}: {e}", dir.display())))?;
            Ok(session)
        }
        SessionSelection::Existing { requested } => {
            // Map the request to the flag that produced it for a helpful message.
            let flag = match requested.as_str() {
                "--continue" => "--continue",
                "--resume" => "--resume",
                _ => "--session",
            };
            Err(BuildError::RestoreNotImplemented {
                requested: requested.clone(),
                flag,
            })
        }
    }
}

/// A fresh ephemeral in-memory session (no persistence). Used for `--no-session`.
fn ephemeral_session() -> Session {
    let storage = Arc::new(InMemorySessionStorage::new(
        SessionMetadata {
            id: "ephemeral".into(),
            created_at: 0,
            parent_session_id: None,
        },
        Arc::new(SystemClock),
        Arc::new(DefaultIdGenerator::new()),
    ));
    Session::new(storage, None)
}

/// Create a fresh JSONL session file under `dir` and wrap it in a `Session`.
///
/// Uses the `JsonlSessionRepo` over an `OsExecutionEnv`-backed `FileSystem`
/// rooted at the cwd, so paths resolve consistently with the tools. Mirrors the
/// TS `SessionManager.create` flow (header write + `JsonlSessionStorage` open).
async fn create_jsonl_session(dir: &Path, cwd: &str) -> Result<Session, String> {
    use rpi_harness::session::jsonl::{
        JsonlSessionCreateOptions, JsonlSessionRepo, JsonlSessionRepoOptions,
    };
    use rpi_tools::FileSystem;

    // A dedicated OS env for session-file I/O, rooted at the cwd so the repo's
    // relative-path resolution matches the tool env.
    let env = Arc::new(OsExecutionEnv::with_cwd(PathBuf::from(cwd)));
    let fs: Arc<dyn FileSystem> = env.clone();

    let repo = JsonlSessionRepo::with_env_cwd(JsonlSessionRepoOptions {
        fs: fs.clone(),
        sessions_root: dir.to_string_lossy().into_owned(),
        clock: Arc::new(SystemClock),
        ids: Arc::new(DefaultIdGenerator::new()),
    });

    let opts = JsonlSessionCreateOptions {
        id: None, // fresh uuidv7
        parent_session_id: None,
        cwd: cwd.to_string(),
        metadata: None,
    };
    let storage = repo
        .create_typed(&opts)
        .await
        .map_err(|e| format!("create session: {e}"))?;
    // `JsonlSessionStorage` implements `SessionStorage`; wrap in the facade.
    let storage_arc: Arc<dyn rpi_harness::session::types::SessionStorage> = Arc::new(storage);
    Ok(Session::new(storage_arc, None))
}

/// Read an `--append-system-prompt` target: if it's a readable file path, return
/// its contents; otherwise return `None` and let the caller use the literal.
fn read_append_target(target: &str) -> Option<String> {
    let path = Path::new(target);
    if path.is_file() {
        std::fs::read_to_string(path).ok()
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::Args;

    #[test]
    fn default_prompt_mentions_cwd_and_tools() {
        let p = default_system_prompt("/tmp/proj");
        assert!(p.contains("/tmp/proj"));
        assert!(p.contains("read"));
        assert!(p.contains("bash"));
        assert!(p.contains("edit"));
        assert!(p.contains("write"));
        assert!(p.contains("grep"));
        assert!(p.contains("find"));
        assert!(p.contains("ls"));
    }

    #[test]
    fn select_ephemeral_when_no_session() {
        let args = Args { no_session: true, ..Args::default() };
        let cwd = Path::new("/tmp");
        assert!(matches!(select_session(&args, cwd), SessionSelection::Ephemeral));
    }

    #[test]
    fn select_existing_for_continue() {
        let args = Args { continue_session: true, ..Args::default() };
        let cwd = Path::new("/tmp");
        assert!(matches!(
            select_session(&args, cwd),
            SessionSelection::Existing { .. }
        ));
    }

    #[test]
    fn select_new_with_custom_dir() {
        let args = Args {
            session_dir: Some(PathBuf::from("/tmp/sess")),
            ..Args::default()
        };
        let cwd = Path::new("/tmp");
        match select_session(&args, cwd) {
            SessionSelection::New { dir, .. } => assert_eq!(dir, PathBuf::from("/tmp/sess")),
            other => panic!("expected New, got {other:?}"),
        }
    }

    #[test]
    fn select_new_default_dir() {
        let args = Args::default();
        let cwd = Path::new("/proj");
        match select_session(&args, cwd) {
            SessionSelection::New { dir, .. } => {
                assert_eq!(dir, Path::new("/proj/.pi/sessions"));
            }
            other => panic!("expected New, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ephemeral_session_builds_roundtrips() {
        // Sanity: the ephemeral path produces a usable Session facade (the
        // harness build itself needs a provider; tested via the integration
        // path in tests/build.rs instead).
        let s = ephemeral_session();
        let leaf = s.get_leaf_id().await;
        assert!(leaf.is_ok());
    }

    // NOTE: `build_tools`/`active_tool_names` integration is exercised by the
    // `tests/build.rs` harness-build test (needs a provider + multi-thread rt).
}
