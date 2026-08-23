//! Harness construction + session-storage wiring. Mirrors the Rust-side
//! equivalent of the TS `packages/coding-agent/src/core/sdk.ts`
//! (`createAgentSession`) — build the env, tools, durable session storage, and
//! `AgentHarnessOptions`, then `AgentHarness::create`.
//!
//! v1 scope cuts vs the TS SDK (tracked in `docs/m6-cli-open-questions.md`):
//! - **Skill / prompt-template / context-file discovery IS wired**
//!   (`--no-skills`/`-ns`, `--no-prompt-templates`/`-np`, `--no-context-files`/
//!   `-nc` each suppress one channel; project `.pi/<sub>` + global
//!   `agent_dir()<sub>` discovery with project-wins dedupe via
//!   [`crate::resource_dirs`]; SYSTEM.md/APPEND_SYSTEM.md project-wins
//!   precedence). **Extension/theme discovery and trust gating remain
//!   deferred** — the harness `resources` carry skills+prompt-templates; the
//!   system prompt adds `<project_context>` + `APPEND_SYSTEM.md` append text.
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
use rpi_harness::context_files::{format_project_context, load_project_context_files};
use rpi_harness::session::memory::{InMemorySessionStorage, SystemClock};
use rpi_harness::session::session::DefaultIdGenerator;
use rpi_harness::session::types::SessionMetadata;
use rpi_harness::session::Session;
use rpi_harness::system_prompt::compose_system_prompt;
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
use crate::resource_dirs::{
    discover_append_system_prompt_file, discover_system_prompt_file, global_dir,
    load_prompt_templates_with_precedence, load_skills_with_precedence, project_dir,
    prompt_template_dirs, skill_dirs,
};
use rpi_extensions::{
    ExtensionEmitter, ExtensionSession, NullDiagnostics, PluginDiagnostics, PluginToolAdapter,
    TeeEmitter, load_session,
};

/// The subdirectory (under both project `.pi/` and global `agent_dir()/`) where
/// rpi scans for cdylib plugins. Mirrors pi's `.pi/extensions`.
const EXTENSIONS_SUBDIR: &str = "extensions";

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
///
/// Returns the harness plus a `broadcast::Receiver<AgentEvent>` carrying the
/// live `AgentEvent` stream from every run (backed by a `BroadcastEmitter`
/// installed on the harness). Interactive mode drains this to render streaming
/// responses; the non-interactive modes simply drop it.
pub async fn build(
    resolved: &ResolvedModel,
    args: &Args,
    cwd: &Path,
) -> Result<
    (
        AgentHarness,
        tokio::sync::broadcast::Receiver<rpi_agent::AgentEvent>,
    ),
    BuildError,
> {
    let cwd_str = cwd.to_string_lossy().to_string();

    // ---- Execution env + tools ----
    let env = Arc::new(OsExecutionEnv::with_cwd(cwd.to_path_buf()));
    let env_dyn: Arc<dyn rpi_tools::ExecutionEnv> = env.clone();
    let mut_env: Arc<dyn rpi_tools::MutatingEnv> = env.clone();
    let _registry = Arc::new(MutationQueueRegistry::new());
    // `env_dyn` is shared between the tool context (moved in) and the resource
    // loaders below (borrowed); clone one branch so both hold a reference.
    let ctx = ExecutionToolContext::new(env_dyn.clone(), Some(mut_env));

    let tools = build_tools(&ctx, args);
    let mut tools = tools;

    // ---- Extensions (Part B2) ----
    // Load cdylib plugins from the resolved extension dirs, merge their tools
    // into the built-in set (extension overrides same-named built-in; first-
    // extension-wins across plugins; explicit `--tools`/`--exclude-tools` still
    // apply to the merged set), and keep the loaded `Library` handles alive for
    // the harness lifetime via the returned session guard. `--no-extensions`
    // skips discovery entirely (no dirs scanned, no plugins loaded).
    let extension_session = if args.no_extensions {
        ExtensionSession::none()
    } else {
        load_extensions(args, cwd)
    };
    if args.verbose {
        if let Some(s) = extension_session.summary() {
            eprintln!("extensions: {s}");
        }
    }
    merge_extension_tools(&mut tools, &extension_session, args);
    let active = active_tool_names(&tools, args);

    // ---- Session storage ----
    let selection = select_session(args, cwd);
    let session = build_session(&selection, &cwd_str).await?;

    // ---- System prompt base (precedence: --system-prompt > SYSTEM.md > default) ----
    // Mirrors pi `discoverSystemPromptFile` (`resource-loader.ts:1022-1034`):
    // an explicit `--system-prompt` flag wins; otherwise a discovered
    // `<cwd>/.pi/SYSTEM.md` (project) overrides `<agent_dir>/SYSTEM.md`
    // (global); otherwise the built-in default. **Project-wins** — the same
    // direction as skills/prompts precedence.
    let base_prompt = match args.system_prompt.as_deref() {
        Some(explicit) => explicit.to_string(),
        None => match discover_system_prompt_file(cwd) {
            Some(path) => std::fs::read_to_string(&path).unwrap_or_else(|_| {
                default_system_prompt(&cwd_str)
            }),
            None => default_system_prompt(&cwd_str),
        },
    };

    // ---- Append-text sources (precedence: --append-system-prompt > APPEND_SYSTEM.md) ----
    // Mirrors pi `appendSystemPrompt` (`resource-loader.ts:525-542`). Explicit
    // `--append-system-prompt` flags are joined together; when none are given, a
    // discovered `APPEND_SYSTEM.md` (project-wins over global) provides the
    // append text. `--append-system-prompt` takes a value that may be a literal
    // string OR a readable file path (mirrors TS `resolvePromptInput`).
    let mut append_texts: Vec<String> = Vec::new();
    for extra in &args.append_system_prompt {
        let text = read_append_target(extra).unwrap_or_else(|| extra.clone());
        append_texts.push(text);
    }
    if args.append_system_prompt.is_empty() {
        if let Some(path) = discover_append_system_prompt_file(cwd) {
            if let Ok(text) = std::fs::read_to_string(&path) {
                append_texts.push(text);
            }
        }
    }
    let append_join = if append_texts.is_empty() {
        None
    } else {
        Some(append_texts.join("\n\n"))
    };

    // ---- Resource discovery (skills + prompt-templates + context-files) ----
    // The env is OS-backed, rooted at cwd. Each `--no-*` flag suppresses its
    // channel independently (pi parity). Skills/prompts load project→global then
    // dedupe first-wins-by-name (project wins). Context files walk
    // global→ancestor(cwd→root), deepest-last (pi parity).
    //
    // **Trust gate (v1 divergence):** pi gates project `.pi/*` discovery on
    // `isProjectTrusted()` (global resources are unconditional). rpi v1 has no
    // trust prompt — project resources are discovered unconditionally (a copied
    // `.pi/` drops in and works). Full trust gating is deferred.
    let agent_dir = crate::config::agent_dir().ok();

    let mut skills: Vec<rpi_harness::types::Skill> = Vec::new();
    let mut skill_diags: Vec<rpi_harness::skills::SkillDiagnostic> = Vec::new();
    if !args.no_skills {
        let dirs = skill_dirs(cwd);
        let result = load_skills_with_precedence(&env_dyn, &dirs).await;
        skills = result.skills;
        skill_diags = result.diagnostics;
    }

    let mut prompt_templates: Vec<rpi_harness::types::PromptTemplate> = Vec::new();
    let mut prompt_diags: Vec<rpi_harness::prompt_templates::PromptTemplateDiagnostic> = Vec::new();
    if !args.no_prompt_templates {
        let dirs = prompt_template_dirs(cwd);
        let result = load_prompt_templates_with_precedence(&env_dyn, &dirs).await;
        prompt_templates = result.prompt_templates;
        prompt_diags = result.diagnostics;
    }

    let context_block = if args.no_context_files {
        String::new()
    } else {
        // `load_project_context_files` walks the global agentDir first then
        // ancestor-walks cwd→root (deepest last). It needs a real agent_dir; if
        // none is resolvable, pass the cwd dir so only the ancestor-walk runs
        // (the global step returns None anyway).
        let agent_dir_path = agent_dir.clone().unwrap_or_else(|| cwd.to_path_buf());
        let files = load_project_context_files(&env_dyn, cwd, &agent_dir_path).await;
        format_project_context(&files)
    };

    // Surface resource-discovery diagnostics as startup warnings (verbose-only).
    if args.verbose {
        for d in &skill_diags {
            eprintln!("warning: skill {} ({}): {}", d.path, d.code.as_str(), d.message);
        }
        for d in &prompt_diags {
            eprintln!(
                "warning: prompt template {} ({}): {}",
                d.path,
                d.code.as_str(),
                d.message
            );
        }
    }

    // ---- Compose the full system prompt ----
    // Order mirrors pi `buildSystemPrompt` (`system-prompt.ts:28-72`):
    // base → append → context → skills. The skills listing is the harness's own
    // section: `AgentHarness::compose_prompt` appends `<available_skills>` (gated
    // on the `read` tool + `disable_model_invocation`, applied inside
    // `format_skills_for_system_prompt`). So we pass None for skills here (the
    // harness adds the listing itself) and fold only base+append+context into
    // the prompt we hand the harness.
    let system_prompt = compose_system_prompt(
        Some(&base_prompt),
        &[], // skills: harness appends the listing itself
        if context_block.is_empty() { None } else { Some(&context_block) },
        append_join.as_deref(),
    );

    // ---- Debug: dump the resolved system-prompt sections (verification) ----
    // A verification affordance for Part-A resource discovery: prints the
    // composed sections + resource counts to stderr so a smoke can confirm
    // `<available_skills>` + `<project_context>` + appended text reached the
    // prompt without parsing a provider round-trip. The harness composes the
    // final prompt (base → append → context → skills); here we print the
    // pre-harness sections (the harness adds the skills listing itself, gated
    // on `read` + `disable_model_invocation`).
    if args.debug_system_prompt {
        eprintln!("=== --debug-system-prompt ===");
        let base_src = if args.system_prompt.is_some() {
            "--system-prompt"
        } else if discover_system_prompt_file(cwd).is_some() {
            "SYSTEM.md"
        } else {
            "default"
        };
        eprintln!("[base source: {base_src}]");
        eprintln!("--- base ---\n{base_prompt}");
        if let Some(append) = append_join.as_deref() {
            eprintln!("--- append ---\n{append}");
        } else {
            eprintln!("--- append: (none) ---");
        }
        if context_block.is_empty() {
            eprintln!("--- context: (none) ---");
        } else {
            eprintln!("--- context ---{context_block}");
        }
        let visible_skills = skills
            .iter()
            .filter(|s| s.disable_model_invocation != Some(true))
            .count();
        eprintln!(
            "--- skills: {} loaded ({} model-visible, {} hidden) ---",
            skills.len(),
            visible_skills,
            skills.len() - visible_skills
        );
        for s in &skills {
            let hidden = if s.disable_model_invocation == Some(true) { " [hidden]" } else { "" };
            eprintln!("    {}{hidden} — {}", s.name, s.description);
        }
        eprintln!("--- prompt templates: {} ---", prompt_templates.len());
        for t in &prompt_templates {
            eprintln!("    /{}", t.name);
        }
        eprintln!(
            "--- final composed base+append+context (skills listing added by harness) ---\n{system_prompt}"
        );
        eprintln!("=== end --debug-system-prompt ===");
    }

    // ---- Options ----
    // Install a BroadcastEmitter so the caller (the interactive TUI) can drain
    // AgentEvents live as a run unfolds. The corresponding broadcast::Receiver
    // is returned alongside the harness; non-interactive modes simply drop it.
    let (broadcast, event_rx) = rpi_agent::events::BroadcastEmitter::new(256);
    let broadcast_emitter: Arc<dyn rpi_agent::AgentEmitter> = Arc::new(broadcast);

    // ---- Extensions emitter (Part B3a) ----
    // If extensions loaded + registered any `on()` handlers, wrap the
    // broadcast emitter in a `TeeEmitter` so every `AgentEvent` flows to BOTH
    // the TUI (via the broadcast receiver above) AND the plugin handlers (via
    // the `ExtensionEmitter`, which translates each `AgentEvent` →
    // `StablePluginEvent` and fans out to the handlers registered for its tag).
    // With no extensions the tee degrades to the bare broadcast emitter (a
    // one-child passthrough), so the TUI path is unchanged.
    let emitter: Arc<dyn rpi_agent::AgentEmitter> =
        match extension_session.snapshot_arc() {
            Some(snapshot) => {
                let ext = ExtensionEmitter::new(snapshot, extension_session.keepalive());
                Arc::new(TeeEmitter::new(vec![
                    broadcast_emitter,
                    Arc::new(ext),
                ]))
            }
            None => broadcast_emitter,
        };

    let options = AgentHarnessOptions {
        model: resolved.model.clone(),
        thinking_level: resolved.thinking_level,
        active_tool_names: active,
        tools,
        system_prompt: Some(system_prompt),
        resources: AgentHarnessResources {
            skills: if skills.is_empty() { None } else { Some(skills) },
            prompt_templates: if prompt_templates.is_empty() {
                None
            } else {
                Some(prompt_templates)
            },
        },
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
        agent_emitter: Some(emitter),
        // B3b: the three exists-but-`None` loop hooks — populated when an
        // extension session registers handlers for the matching pi `on()`
        // tags (before_tool_call/after_tool_call/context). v1 leaves them `None`
        // here; the rpi-extensions adapter that owns plugin handler dispatch is
        // wired in the same build path once B3b's host-side adapter lands.
        before_tool_call: None,
        after_tool_call: None,
        transform_context: None,
        entry_transforms: Vec::new(),
    };

    AgentHarness::create(options)
        .await
        .map(|harness| (harness, event_rx))
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

/// Resolve the extension dirs to scan and load the cdylib plugins, returning
/// the loaded session guard (keeps the `Library` handles alive for the harness
/// lifetime). Scan order: project `.pi/extensions`, global `agent_dir()/`
/// `extensions`, then any `--extensions-dir` flags (scanned after the defaults
/// — `args.rs`). Diagnostics are a no-op sink for now; load skips/ABI mismatches
/// surface via the `--verbose` summary.
fn load_extensions(args: &Args, cwd: &Path) -> ExtensionSession {
    let mut dirs = vec![project_dir(cwd, EXTENSIONS_SUBDIR)];
    if let Some(g) = global_dir(EXTENSIONS_SUBDIR) {
        dirs.push(g);
    }
    dirs.extend(args.extensions_dir.iter().cloned());
    let diagnostics: Arc<dyn PluginDiagnostics> = Arc::new(NullDiagnostics);
    load_session(&dirs, diagnostics)
}

/// Merge the loaded extension tools into the built-in set. An extension tool
/// overrides a same-named built-in; first-extension-wins across plugins is
/// already guaranteed by the registry (`register_tool` keeps the prior). The
/// explicit `--tools` allowlist / `--exclude-tools` denylist apply to the
/// merged set (the built-ins were already filtered in [`build_tools`]).
fn merge_extension_tools(tools: &mut Vec<HarnessTool>, session: &ExtensionSession, args: &Args) {
    let Some(snapshot) = session.snapshot() else { return };
    for et in snapshot.tools() {
        let name = &et.tool.name;
        if let Some(allow) = &args.tools {
            if !allow.iter().any(|a| a == name) {
                continue;
            }
        }
        if let Some(deny) = &args.exclude_tools {
            if deny.iter().any(|d| d == name) {
                continue;
            }
        }
        let adapter = PluginToolAdapter::new(et.tool.clone(), et.handle(), session.keepalive());
        let harness_tool = HarnessTool::new(Arc::new(adapter));
        match tools.iter_mut().find(|t| t.tool.schema().name == *name) {
            Some(slot) => *slot = harness_tool,
            None => tools.push(harness_tool),
        }
    }
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
