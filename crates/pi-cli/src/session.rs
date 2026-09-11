//! Harness construction + session-storage wiring. Mirrors the Rust-side
//! equivalent of the TS `packages/coding-agent/src/core/sdk.ts`
//! (`createAgentSession`) — build the env, tools, durable session storage, and
//! `AgentHarnessOptions`, then `AgentHarness::create`.
//!
//! v1 scope cuts vs the TS SDK (tracked in `docs/m6-cli-open-questions.md`):
//! - **Skill / prompt-template / context-file discovery IS wired**
//!   (`--no-skills`/`-ns`, `--no-prompt-templates`/`-np`, `--no-context-files`/
//!   `-nc` each suppress one channel; project `.rpi/<sub>` + legacy
//!   `.pi/<sub>` + global
//!   `agent_dir()<sub>` discovery with project-wins dedupe via
//!   [`crate::resource_dirs`]; SYSTEM.md/APPEND_SYSTEM.md project-wins
//!   precedence). **Extension `resources_discover` (B5b) feeds the SAME loaders:
//!   a plugin's discovered skill/prompt paths merge with the static dirs and
//!   re-run through `load_skills`/`load_prompt_templates` (individual `.md` files
//!   load too — `load_skills` accepts both dirs and files). Package themes are
//!   parsed by the TUI when selected via settings or `--theme`.**
//!   **Trust gating remains deferred** — project resources are discovered
//!   unconditionally (a copied `.rpi/` or legacy `.pi/` drops in and works).
//! - **No `--models` cycling, no `ModelRuntime`/multi-provider.** One model,
//!   one provider (Anthropic), resolved up-front by [`crate::provider`].
//! - **Built-in tools**: `read`, `bash`, `edit`, `write` plus the read-only
//!   `grep`/`find`/`ls` (the TS `createCodingTools` default set). `grep`/`find`
//!   use an in-process `FileSystem`+`regex`/`globset` implementation (documented
//!   divergence from the TS `rg`/`fd` shell-out; see `docs/m4-tools-open-questions.md`).
//! - **Session restore (`-c`/`-r`/`--session`)** is *partially* supported: a
//!   fresh session is always created. The harness's `create` rejects sessions
//!   that already have records unless `allow_existing_session` is enabled.
//!   The interactive `-c`/`-r`/`--session` paths enable that mode and replay
//!   the existing branch before appending new messages. See [`SessionSelection`].

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use rpi_agent::AgentTool;
use rpi_ai::Provider;
use rpi_harness::agent_harness::AgentHarness;
use rpi_harness::context_files::{format_project_context, load_project_context_files};
use rpi_harness::session::memory::{InMemorySessionStorage, SystemClock};
use rpi_harness::session::session::DefaultIdGenerator;
use rpi_harness::session::types::{BranchBounds, EntryQuery, SessionMetadata};
use rpi_harness::session::Session;
use rpi_harness::system_prompt::compose_system_prompt;
use rpi_harness::types::{
    AgentHarnessOptions, AgentHarnessResources, CompactionSettings, DrivingMode, HarnessTool,
    HarnessToolExecution, RetryPolicy, ToolReplay,
};
use rpi_tools::{
    create_bash_tool, create_edit_tool, create_find_tool, create_grep_tool, create_ls_tool,
    create_powershell_tool, create_read_tool, create_write_tool, ExecutionToolContext,
    MutationQueueRegistry, OsExecutionEnv,
};

use crate::args::Args;
use crate::extension_api::ExtensionBackend;
use crate::provider::ResolvedModel;
use crate::resource_dirs::{
    discover_append_system_prompt_file_with_packages, discover_system_prompt_file_with_packages,
    global_dir, load_prompt_templates_with_precedence, load_skills_with_precedence, project_dirs,
    prompt_template_dirs, skill_dirs,
};
use rpi_extensions::{
    emit_resources_discover, ExtensionEmitter, ExtensionSession, NullDiagnostics,
    PluginDiagnostics, PluginToolAdapter, TeeEmitter,
};

/// The subdirectory (under project `.rpi/`, legacy `.pi/`, and global
/// `agent_dir()`) where rpi scans for cdylib plugins.
const EXTENSIONS_SUBDIR: &str = "extensions";

/// The built-in tool names v1 ships, in the order the TS `createCodingTools`
/// registers them: the mutating set (`read`/`bash`/`edit`/`write`) followed by
/// the read-only search set (`grep`/`find`/`ls`).
pub const BUILTIN_TOOL_NAMES: &[&str] = &[
    "read",
    "bash",
    "edit",
    "write",
    "grep",
    "find",
    "ls",
    "powershell",
];

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
- powershell — Execute PowerShell commands on Windows

Guidelines:
- Be concise in your responses
- Show file paths clearly when working with files
- Prefer the smallest change that solves the problem

Current working directory: {cwd}"
    )
}

/// How the user asked to select a session. v1 honors `NoSession` (ephemeral
/// `InMemorySessionStorage`), `New` (a fresh JSONL file), and — new this pass —
/// `Latest` / `ById`, which **restore** an existing JSONL session on launch
/// (`--continue`/`-c`, `--resume`/`-r`, `--session <id|path>`). The restored
/// transcript renders into the TUI on startup and the run continues appending
/// to the same file.
#[derive(Debug, Clone)]
pub enum SessionSelection {
    /// `--no-session`: ephemeral, in-memory, nothing persisted.
    Ephemeral,
    /// Fresh durable JSONL session under `--session-dir` (or the default dir).
    New { dir: PathBuf, name: Option<String> },
    /// `-c` / `-r`: restore the most recent session in the default dir.
    Latest,
    /// `--session <id|path>`: restore the session whose id matches, or whose
    /// file name contains the id.
    ById { id: String },
    /// `--session-id <id>`: use the EXACT session id, creating it if missing.
    ByExactId { id: String },
    /// `--fork <path|id>`: fork the given session into a new one and start in
    /// the fork.
    Fork { source: String },
}

/// Decide the session selection from parsed args + the resolved cwd.
pub fn select_session(args: &Args, cwd: &Path) -> SessionSelection {
    if args.no_session {
        return SessionSelection::Ephemeral;
    }
    if args.continue_session || args.resume {
        // `--continue` and `--resume` both restore the most recent session.
        return SessionSelection::Latest;
    }
    if let Some(s) = &args.fork {
        return SessionSelection::Fork { source: s.clone() };
    }
    if let Some(s) = &args.session_id {
        return SessionSelection::ByExactId { id: s.clone() };
    }
    if let Some(s) = &args.session {
        return SessionSelection::ById { id: s.clone() };
    }
    let dir = args
        .session_dir
        .clone()
        .unwrap_or_else(|| default_session_dir(cwd));
    SessionSelection::New {
        dir,
        name: args.name.clone(),
    }
}

/// The default session directory: prefer `<cwd>/.rpi/sessions`, while keeping
/// an existing `<cwd>/.pi/sessions` directory usable for compatibility. A new
/// project therefore starts with the rpi-owned directory.
pub fn default_session_dir(cwd: &Path) -> PathBuf {
    let preferred = cwd.join(".rpi").join("sessions");
    let legacy = cwd.join(".pi").join("sessions");
    if preferred.exists() || !legacy.exists() {
        preferred
    } else {
        legacy
    }
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
/// Returns the harness, the live `AgentEvent` broadcast receiver, and a
/// [`ReloadContext`] the interactive TUI holds to drive `/reload` (and a
/// plugin's `runtime_action(Reload)` via the mailbox). Non-interactive modes
/// drop the context (no `/reload` surface in print/json mode).
pub async fn build(
    resolved: &ResolvedModel,
    args: &Args,
    cwd: &Path,
) -> Result<
    (
        AgentHarness,
        tokio::sync::broadcast::Receiver<rpi_agent::AgentEvent>,
        ReloadContext,
    ),
    BuildError,
> {
    let cwd_str = cwd.to_string_lossy().to_string();
    let package_resources = crate::packages::discover_from_settings(cwd);

    // ---- B5a: build the action bridge BEFORE extension load ----
    // Extensions load before `AgentHarness::create` (extensions provide tools the
    // harness is built with), but a plugin stores the `ActionBridge`'s raw
    // `user_data` pointer during `register` and it must remain valid + the host
    // must be ready for the whole session. So:
    //  1. Capture the current tokio `Handle` (the async main-thread runtime) —
    //     the bridge spawns dispatch from any thread via `Handle::spawn`.
    //  2. Build an *empty* `HarnessActionHost` (its harness cell is unset; no
    //     plugin can call a runtime action before the harness runs).
    //  3. Wrap it as `Arc<dyn RuntimeActionHost>` + `ActionBridge`, thread
    //     `Some(bridge)` into `load_extensions` so every plugin's `user_data`
    //     points at this bridge.
    //  4. After `AgentHarness::create` succeeds, call `set_harness(&cell, …)` to
    //     fill the host cell the bridge recovers on the first action call.
    let runtime = tokio::runtime::Handle::try_current().map_err(|e| {
        BuildError::HarnessCreate(format!("no tokio runtime for action bridge: {e}"))
    })?;
    let catalog = crate::provider::available_catalog(resolved);
    let (action_host, harness_cell) = crate::extensions_actions::HarnessActionHost::new_empty(
        catalog.clone(),
        cwd.to_path_buf(),
        runtime.clone(),
    );
    let host_arc: Arc<dyn rpi_extensions::RuntimeActionHost> = Arc::new(action_host);
    // `runtime` is reused below (B5c: `PluggableProvider` needs a captured
    // `Handle` to `spawn_blocking` the sync `ProviderRequestFn`), so clone here.
    //
    // B5d: build the initial bridge WITH a reload callback backed by a session-
    // long `ReloadMailbox` (cloned into `ReloadContext` + handed to the TUI). A
    // plugin's `runtime_action(Reload)` then signals the TUI's main loop instead
    // of hitting the "not configured" fallback. The same mailbox is reused on
    // `/reload` (the fresh bridge carries `ctx.mailbox`), so the bridge always
    // points at the one TUI-installed sender across reloads.
    let reload_mailbox = rpi_extensions::ReloadMailbox::new();
    let action_bridge = rpi_extensions::ActionBridge::with_reload(
        runtime.clone(),
        host_arc,
        rpi_extensions::reload_callback_from_mailbox(reload_mailbox.clone()),
    );

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
        load_extensions(args, cwd, Some(Arc::clone(&action_bridge)))
    };
    let js_extension_session = if args.no_extensions {
        None
    } else {
        let paths = js_extension_paths(args, cwd, &package_resources);
        let js_context = serde_json::json!({
            "cwd": cwd_str,
            "theme": resolved.theme.clone(),
            "currentModel": resolved.model.clone(),
            "models": catalog.clone(),
            "thinkingLevel": resolved.thinking_level,
        });
        match crate::js_extensions::JsExtensionSession::load_with_context(
            &paths,
            args.verbose,
            js_context,
        ) {
            Ok(session) => session,
            Err(error) => {
                eprintln!("warning: JS/TS extensions were not loaded: {error}");
                None
            }
        }
    };
    if js_extension_session.is_some() {
        eprintln!(
            "warning: enabled Pi JS/TS extensions execute with the current user's permissions"
        );
    }
    if let Some(session) = &js_extension_session {
        if let Err(error) =
            session.enable_provider_runtime(resolved.provider.clone(), runtime.clone())
        {
            if args.verbose {
                eprintln!("warning: JS provider runtime was not enabled: {error}");
            }
        }
    }
    if args.verbose {
        if let Some(session) = &js_extension_session {
            let info = session.backend_info();
            eprintln!(
                "JS extension backend: {} v{} ({})",
                info.name,
                info.api_version,
                info.capability_names().join(", ")
            );
        }
        if let Some(s) = extension_session.summary() {
            eprintln!("extensions: {s}");
        }
        report_deferred_renderers(&extension_session);
    }
    merge_extension_tools(&mut tools, &extension_session, args);
    if let Some(session) = &js_extension_session {
        merge_js_extension_tools(&mut tools, session, args);
        if args.verbose && !session.commands.is_empty() {
            eprintln!("JS extension commands: {}", session.commands.join(", "));
        }
    }
    let active = active_tool_names(&tools, args);

    // ---- Session storage ----
    let selection = select_session(args, cwd);
    let session = build_session(&selection, &cwd_str).await?;
    if let Some(js) = &js_extension_session {
        let session_id = session
            .get_metadata()
            .await
            .ok()
            .map(|metadata| metadata.id);
        let leaf_id = session.get_leaf_id().await.ok().flatten();
        if let Some(session_id) = session_id {
            let branch = session
                .find_entries_on_branch(&EntryQuery::default(), &BranchBounds::default())
                .await
                .ok()
                .unwrap_or_default();
            let branch_json =
                serde_json::to_value(&branch).unwrap_or_else(|_| serde_json::json!([]));
            let runtime_context = serde_json::json!({
                "session": {
                    "id": session_id,
                    "leafId": leaf_id,
                    "branch": branch_json,
                    "entries": branch_json.clone(),
                },
            });
            if let Err(error) = js.set_runtime_context(runtime_context) {
                if args.verbose {
                    eprintln!("warning: could not sync JS session context: {error}");
                }
            }
        }
    }

    // ---- System prompt base (precedence: --system-prompt > SYSTEM.md > default) ----
    // Mirrors pi `discoverSystemPromptFile` (`resource-loader.ts:1022-1034`):
    // an explicit `--system-prompt` flag wins; otherwise a discovered
    // `<cwd>/.rpi/SYSTEM.md` wins, then legacy `<cwd>/.pi/SYSTEM.md`, then
    // `<agent_dir>/SYSTEM.md`.
    // (global); otherwise the built-in default. **Project-wins** — the same
    // direction as skills/prompts precedence.
    let base_prompt = match args.system_prompt.as_deref() {
        Some(explicit) => explicit.to_string(),
        None => match discover_system_prompt_file_with_packages(cwd, &package_resources) {
            Some(path) => {
                std::fs::read_to_string(&path).unwrap_or_else(|_| default_system_prompt(&cwd_str))
            }
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
        if let Some(path) =
            discover_append_system_prompt_file_with_packages(cwd, &package_resources)
        {
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
    // channel independently (pi parity). Skills/prompts load project→global,
    // explicit/plugin paths, then static packages; dedupe first-wins-by-name
    // keeps project and user resources ahead of packages. Context files walk
    // global→ancestor(cwd→root), deepest-last (pi parity).
    //
    // **Trust gate (v1 divergence):** pi gates project config discovery on
    // `isProjectTrusted()` (global resources are unconditional). rpi v1 has no
    // trust prompt — project resources are discovered unconditionally (a copied
    // `.rpi/` or `.pi/` drops in and works). Full trust gating is deferred.
    let agent_dir = crate::config::agent_dir().ok();

    // ---- B5b: extension resources_discover ----
    // If any plugin registered a `resources_discover` handler, fan the event out
    // (reason "startup") and collect skill/prompt/theme paths. These plugin-
    // contributed paths merge WITH the static Part-A dirs (project
    // `.rpi/skills`, legacy `.pi/skills` +
    // `agent_dir/skills`, etc.) and the loaders re-run over the union — the
    // coherence point: a plugin's discovered skills land through the SAME loaders
    // as static skills. Static dirs load FIRST so project skills keep winning name
    // collisions (a plugin must not shadow a project skill of the same name —
    // mirrors pi `extendResources` running AFTER the default load's first-wins
    // map). `load_skills` now accepts both dirs and individual `.md` files, so a
    // plugin returning bare `SKILL.md` paths loads them (the gap this closes).
    // Theme paths are available to the TUI through the package resource list;
    // skill/prompt loaders are the only resources needed by the harness here.
    // A `--no-*` flag suppresses its channel for BOTH static and discovered paths.
    let discovered = extension_session
        .snapshot_arc()
        .map(|snap| emit_resources_discover(&cwd_str, "startup", &snap))
        .unwrap_or_default();

    let mut skills: Vec<rpi_harness::types::Skill> = Vec::new();
    let mut skill_diags: Vec<rpi_harness::skills::SkillDiagnostic> = Vec::new();
    if !args.no_skills {
        let mut dirs = skill_dirs(cwd);
        dirs.extend(args.skill.iter().cloned());
        dirs.extend(discovered.skill_paths.iter().map(PathBuf::from));
        if let Some(session) = &js_extension_session {
            dirs.extend(session.resources.skill_paths.iter().cloned());
        }
        dirs.extend(package_resources.skill_dirs());
        let result = load_skills_with_precedence(&env_dyn, &dirs).await;
        skills = result.skills;
        skill_diags = result.diagnostics;
    }

    let mut prompt_templates: Vec<rpi_harness::types::PromptTemplate> = Vec::new();
    let mut prompt_diags: Vec<rpi_harness::prompt_templates::PromptTemplateDiagnostic> = Vec::new();
    if !args.no_prompt_templates {
        let mut dirs = prompt_template_dirs(cwd);
        dirs.extend(args.prompt_template.iter().cloned());
        dirs.extend(discovered.prompt_paths.iter().map(PathBuf::from));
        if let Some(session) = &js_extension_session {
            dirs.extend(session.resources.prompt_paths.iter().cloned());
        }
        dirs.extend(package_resources.prompt_dirs());
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
        for d in &package_resources.diagnostics {
            eprintln!("warning: package {}: {}", d.spec, d.message);
        }
        for d in &skill_diags {
            eprintln!(
                "warning: skill {} ({}): {}",
                d.path,
                d.code.as_str(),
                d.message
            );
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
        if context_block.is_empty() {
            None
        } else {
            Some(&context_block)
        },
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
        } else if discover_system_prompt_file_with_packages(cwd, &package_resources).is_some() {
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
            let hidden = if s.disable_model_invocation == Some(true) {
                " [hidden]"
            } else {
                ""
            };
            eprintln!("    {}{hidden} — {}", s.name, s.description);
        }
        eprintln!("--- prompt templates: {} ---", prompt_templates.len());
        for t in &prompt_templates {
            eprintln!("    /{}", t.name);
        }
        // B5b: surface plugin-contributed discovery paths so a smoke can confirm
        // the resources_discover round-trip fed the loaders; package themes are
        // selected by the TUI rather than injected into the harness prompt.
        eprintln!(
            "--- discovered via resources_discover: {} skill(s), {} prompt(s), {} theme(s) ---",
            discovered.skill_paths.len(),
            discovered.prompt_paths.len(),
            discovered.theme_paths.len(),
        );
        for p in &discovered.skill_paths {
            eprintln!("    skill: {p}");
        }
        for p in &discovered.prompt_paths {
            eprintln!("    prompt: {p}");
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
    // The broadcast half stays live for the whole session (the TUI's drain task
    // holds the receiver); reload re-wraps it in a fresh `TeeEmitter`, so keep
    // a clone for the `ReloadContext` before the tee match consumes the original.
    let broadcast_for_context: Arc<dyn rpi_agent::AgentEmitter> = Arc::clone(&broadcast_emitter);

    // ---- Extensions emitter (Part B3a) ----
    // If extensions loaded + registered any `on()` handlers, wrap the
    // broadcast emitter in a `TeeEmitter` so every `AgentEvent` flows to BOTH
    // the TUI (via the broadcast receiver above) AND the plugin handlers (via
    // the `ExtensionEmitter`, which translates each `AgentEvent` →
    // `StablePluginEvent` and fans out to the handlers registered for its tag).
    // With no extensions the tee degrades to the bare broadcast emitter (a
    // one-child passthrough), so the TUI path is unchanged.
    let emitter: Arc<dyn rpi_agent::AgentEmitter> = match extension_session.snapshot_arc() {
        Some(snapshot) => {
            let ext = ExtensionEmitter::new(snapshot, extension_session.keepalive());
            Arc::new(TeeEmitter::new(vec![broadcast_emitter, Arc::new(ext)]))
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
            skills: if skills.is_empty() {
                None
            } else {
                Some(skills)
            },
            prompt_templates: if prompt_templates.is_empty() {
                None
            } else {
                Some(prompt_templates)
            },
        },
        // A restored session (--continue/--resume/--session) already has
        // records — let the harness load it and keep appending.
        allow_existing_session: matches!(
            selection,
            SessionSelection::Latest
                | SessionSelection::ById { .. }
                | SessionSelection::ByExactId { .. }
                | SessionSelection::Fork { .. }
        ),
        stream_options: Default::default(),
        retry: RetryPolicy::default(),
        compaction: CompactionSettings::default(),
        steering_mode: Default::default(),
        follow_up_mode: Default::default(),
        tool_execution: HarnessToolExecution::default(),
        drive: DrivingMode::default(),
        session,
        // B5c: inject the resolved gateway provider PLUS one `Arc<dyn Provider>`
        // per registered extension provider (`PluggableProvider` wraps a plugin's
        // sync `ProviderRequestFn`). The harness's `build_stream_fn` resolves a
        // provider lazily per call by `models.iter().find(|p| p.id() == model.provider)`,
        // so a catalog model whose `provider` matches an extension provider's id
        // routes to it. Extension providers land AFTER the gateway so the gateway
        // stays first-match for its own ids (first-wins on a `.find`).
        models: build_models_with_extensions(resolved, &extension_session, runtime.clone()),
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
        // Extension provider hooks (B4): plugins subscribing to the
        // BeforeProviderRequest / BeforeProviderHeaders / AfterProviderResponse
        // events observe every provider call (observer semantics — the handler
        // ABI has no patch channel in v1). A session without provider-hook
        // subscribers runs hook-free.
        provider_hooks: rpi_extensions::ExtensionProviderHooks::from_session(&extension_session)
            .map(|h| Arc::new(h) as Arc<dyn rpi_ai::ProviderHooks>),
    };

    let harness = match AgentHarness::create(options).await {
        Ok(h) => {
            // Fill the extension action host now that the harness exists
            // (plugin runtime_action calls can then reach it).
            crate::extensions_actions::HarnessActionHost::set_harness(
                &harness_cell,
                Arc::new(h.clone()),
            );
            h
        }
        Err(e) => return Err(BuildError::HarnessCreate(e.to_string())),
    };

    // ---- B5d: assemble the ReloadContext the TUI holds ----
    // Every field is cheap to clone (Arc / Vec / args Clone). The cells own the
    // live session + bridge so `/reload` can swap them; the harness itself is
    // NOT held here (the TUI already owns a `&AgentHarness` / clone at the call
    // site — passing it into `reload_extension_resources` keeps this structfree
    // of a harness back-reference so it can be `Clone` into the reload callback).
    let reload_context = ReloadContext {
        extension_session: Arc::new(Mutex::new(extension_session)),
        js_extension_session: js_extension_session.clone(),
        action_bridge: Arc::new(Mutex::new(Some(Arc::clone(&action_bridge)))),
        catalog,
        gateway: resolved.provider.clone(),
        runtime: runtime.clone(),
        cwd: cwd.to_path_buf(),
        args: args.clone(),
        resolved_model: resolved.model.clone(),
        broadcast: broadcast_for_context,
        mailbox: reload_mailbox,
    };

    Ok((harness, event_rx, reload_context))
}

// ===========================================================================
// B5d — `/reload`: re-run extension + resource discovery into a LIVE harness
// ===========================================================================
//
// `/reload` (interactive TUI command, or a plugin's `runtime_action(Reload)`)
// re-runs everything `build` did around resources/extensions WITHOUT rebuilding
// the `AgentHarness` itself (rebuilding would tear down the session/lane/event
// wiring + the broadcast drain task the TUI owns). Instead it:
//
//  1. Builds a fresh `ExtensionSession` (re-load the cdylibs) over the same
//     dir set, with a FRESH `ActionBridge` (the old one is `invalidate`d so
//     in-flight plugin→host calls on the old bridge fail fast).
//  2. Fans `resources_discover(_, "reload")` over the fresh snapshot.
//  3. Re-runs the Part-A loaders (skills/prompts/context/SYSTEM.md/
//     APPEND_SYSTEM.md) with the discovered paths merged in — same precedence
//     + `--no-*` gates as startup.
//  4. Rebuilds the harness's live state via the B5d setters
//     (`set_system_prompt`/`set_resources`/`set_agent_emitter`/`set_models`/
//     `set_provider_hooks`/`set_tools`) so the NEXT run observes the reloaded
//     config (in-flight runs finish on the old `ConfigSnapshot`).
//  5. Swaps the cells (`ExtensionSession`, `ActionBridge`, harness action
//     host's harness cell stays — the harness is the same object) and drops
//     the old session + bridge (their keepalives unmap the old cdylibs; the
//     new session's keepalive holds the fresh mappings).
//
// The reload is a `rpi-cli` concern (NOT a harness op): `rpi-extensions`
// carries only the `ActionBridge` staleness flag + a `ReloadMailbox` `()` signal
// (no pi-cli `TuiMessage` type — leaf DAG preserved). The TUI owns the mailbox
// receiver + the actual reload routine; a plugin's
// `runtime_action(Reload)` signals the mailbox and returns `Ok(null)`
// immediately so the calling plugin's cdylib is NOT unmapped while its
// `runtime_action` frame is still on the stack (the self-unmapping race a
// synchronous plugin-initiated reload would have).
//
// `reload_extension_resources` is the shared routine both `/reload` (TUI) and
// a plugin's `runtime_action(Reload)` (via the mailbox) drive. It is `pub` so
// the TUI's main-loop handler + the mailbox-driven path call the same code.

/// The cell that holds the live `ExtensionSession` across a `/reload`. Cloned
/// into every site that needs the current session (the TUI, the reload
/// callback). On reload the old session is `replace`d out (its `active` flag
/// flipped + its keepalive dropped, unmapping the old cdylibs) and the fresh one
/// `store`d. Carried as a plain `ExtensionSession` (not `Option`) — a `none()`
/// placeholder fills the slot while the fresh one is being built.
pub type ExtensionSessionCell = Arc<Mutex<ExtensionSession>>;

/// The cell that holds the live `ActionBridge` across a `/reload`. A plugin
/// stores the bridge's raw `user_data` pointer during `register`; on reload the
/// old bridge is `invalidate`d (in-flight calls fail fast) and the fresh one
/// `store`d. The fresh session's plugins are handed the fresh bridge pointer.
pub type ActionBridgeCell = Arc<Mutex<Option<Arc<rpi_extensions::ActionBridge>>>>;

/// Everything `/reload` needs to rebuild extension + resource state into a live
/// harness. Built once in [`build`] (alongside the harness) and held by the TUI
/// (cloned into the reload callback the bridge carries + the `/reload` command
/// handler). The harness itself is NOT held here — the TUI already owns a
/// `&AgentHarness` / a clone; passing it at the call site keeps this struct
/// free of a harness back-reference (so it can be `Clone` and moved into the
/// reload callback without borrowing the harness).
#[derive(Clone)]
pub struct ReloadContext {
    /// The live extension-session cell (swapped on reload).
    pub extension_session: ExtensionSessionCell,
    /// JS/TS Pi extension host kept alive for the interactive session.
    pub js_extension_session: Option<crate::js_extensions::JsExtensionSession>,
    /// The live action-bridge cell (swapped + old invalidated on reload).
    pub action_bridge: ActionBridgeCell,
    /// The model catalog (read-only) the host uses to resolve `set_model(id)`.
    /// `available_catalog(resolved)` is captured once — reload does not re-resolve
    /// the provider (auth/provider resolution is a startup concern; reloading
    /// extensions does not re-open auth).
    pub catalog: Vec<rpi_ai::Model>,
    /// The resolved gateway provider clone (for rebuilding `models` =
    /// `vec![gateway] + PluggableProvider::from_session`). Cheap to clone (`Arc`).
    pub gateway: Arc<dyn Provider>,
    /// The ambient runtime handle (captured in `build`) — `PluggableProvider`
    /// + the fresh `ActionBridge` need a captured `Handle` to spawn from any
    /// thread.
    pub runtime: tokio::runtime::Handle,
    /// The cwd (for static resource-dir resolution + context-file walk).
    pub cwd: PathBuf,
    /// The parsed args (cloned) — `--no-*`/`--tools`/`--exclude-tools`/
    /// `--extensions-dir`/`--no-extensions`/`--system-prompt`/etc all apply on
    /// reload exactly as at startup (a reload re-reads the same flags; it does
    /// not pick up argv changes mid-session, which is the right contract — pi's
    /// `/reload` re-runs discovery with the same config).
    pub args: Args,
    /// The resolved model + thinking level (the harness's active model stays
    /// unless `set_model` changed it; reload does not touch the model).
    pub resolved_model: rpi_ai::Model,
    /// The broadcast emitter the harness was built with. Reload rebuilds the
    /// `TeeEmitter` over the fresh `ExtensionEmitter` (the old tee's extension
    /// child is dropped, unsubscribing from the old registry). The broadcast
    /// half stays live the whole session (the TUI's drain task holds the
    /// receiver), so we keep a handle to re-wrap.
    pub broadcast: Arc<dyn rpi_agent::AgentEmitter>,
    /// The session-long reload mailbox (B5d). Build creates one, installs it on
    /// the initial `ActionBridge` via [`reload_callback_from_mailbox`], and hands
    /// a clone to the TUI. The TUI installs its `TuiMessage` sender so a plugin's
    /// `runtime_action(Reload)` signals the main loop — the reload routine reuses
    /// THIS mailbox (not a fresh default) when building the fresh bridge, so the
    /// bridge always carries the mailbox the TUI installed across reloads.
    pub mailbox: rpi_extensions::ReloadMailbox,
}

/// The outcome of a reload: a human-readable status line for the transcript
/// (counts of what reloaded), and whether any load diagnostics appeared.
pub struct ReloadOutcome {
    /// One-line summary for the transcript note (e.g. "Reloaded 2 plugin(s),
    /// 5 skill(s), 1 prompt(s).").
    pub summary: String,
    /// True iff at least one extension load warning fired (ABI mismatch / skip).
    pub had_warnings: bool,
}

/// Re-run extension + resource discovery and push the rebuilt state into the
/// live `harness` via the B5d setters. The old `ExtensionSession` +
/// `ActionBridge` are invalidated + swapped in [`ReloadContext`]'s cells. This
/// is the single routine both `/reload` (TUI) and a plugin's
/// `runtime_action(Reload)` drive (the latter via the mailbox signal).
///
/// Returns a [`ReloadOutcome`] for the transcript. Best-effort: a failure in
/// one channel (e.g. a plugin that fails to reload) does not abort the others —
/// the reload completes with whatever loaded, mirroring pi's per-plugin
/// skip-on-error. A hard failure (e.g. the harness is closed) surfaces as an
/// error summary.
pub async fn reload_extension_resources(
    harness: &AgentHarness,
    ctx: &ReloadContext,
) -> ReloadOutcome {
    let cwd_str = ctx.cwd.to_string_lossy().to_string();
    let package_resources = crate::packages::discover_from_settings(&ctx.cwd);
    let mut warnings = false;

    // ---- 1. Build a fresh ActionBridge + ExtensionSession ----
    // The fresh bridge carries the SAME `HarnessActionHost` (the host's harness
    // cell already points at this harness; the host impl is reusable across
    // reloads — only the bridge's staleness flag + reload callback differ). We
    // re-use the host by reading it off the OLD bridge (it's the same
    // `Arc<dyn RuntimeActionHost>`).
    let old_bridge = ctx.action_bridge.lock().unwrap().clone();
    let host: Arc<dyn rpi_extensions::RuntimeActionHost> = match &old_bridge {
        Some(b) => b.clone_host(),
        None => {
            // No prior bridge (no extensions ever loaded). Build a fresh host so
            // a reload that newly discovers plugins can still drive actions.
            let (action_host, _cell) = crate::extensions_actions::HarnessActionHost::new_empty(
                ctx.catalog.clone(),
                ctx.cwd.clone(),
                ctx.runtime.clone(),
            );
            crate::extensions_actions::HarnessActionHost::set_harness(
                &_cell,
                Arc::new(harness.clone()),
            );
            Arc::new(action_host)
        }
    };

    let reload_cb = rpi_extensions::reload_callback_from_mailbox(ctx.mailbox.clone());
    let fresh_bridge =
        rpi_extensions::ActionBridge::with_reload(ctx.runtime.clone(), host, reload_cb);

    let extension_session = if ctx.args.no_extensions {
        rpi_extensions::ExtensionSession::none()
    } else {
        load_extensions(&ctx.args, &ctx.cwd, Some(Arc::clone(&fresh_bridge)))
    };
    if extension_session.is_empty() && !ctx.args.no_extensions {
        // The fresh session may be empty if no cdylibs are present — not a
        // warning per se, but note it.
    }
    if ctx.args.verbose {
        if let Some(s) = extension_session.summary() {
            eprintln!("reload: {s}");
        }
        report_deferred_renderers(&extension_session);
    }

    // ---- 2. Invalidate the old session + bridge BEFORE the swap ----
    // The old registry's `active` flag flips false so any in-flight
    // `emit_resources_discover`/event dispatch on the old snapshot no-ops; the
    // old bridge's flag flips false so in-flight `runtime_action` calls parked
    // on the old `user_data` hit the staleness guard. We do this BEFORE storing
    // the fresh session so there is no window where both are "active".
    //
    // The session cell carries a plain `ExtensionSession` (not `Option`), so we
    // `mem::replace` the live one out with a `none()` placeholder to extract it
    // for invalidation (the snapshot's `active` flag is on a shared `Arc`, so a
    // borrow of the extracted value is enough to flip it; the extraction itself
    // also drops the old keepalive once we drop `old_session`, unmapping the old
    // cdylibs). `mem::replace` (not `.take()`) because the cell is not `Option`.
    {
        let mut session_guard = ctx.extension_session.lock().unwrap();
        let old_session = std::mem::replace(
            &mut *session_guard,
            rpi_extensions::ExtensionSession::none(),
        );
        if let Some(old_snap) = old_session.snapshot_arc() {
            // `invalidate` is on the registry, but the snapshot shares the flag —
            // flipping the snapshot's flag invalidates the registry too (same Arc).
            // `RegistrySnapshot` exposes `active_flag()` for this.
            old_snap.active_flag().store(false, Ordering::SeqCst);
        }
        // `old_session` drops here — its keepalive releases the old `Library`
        // handles (unmapping the old cdylibs). The fresh session's keepalive
        // (built below) holds the fresh mappings.
    }
    if let Some(old_b) = old_bridge {
        old_b.invalidate();
    }

    // The fresh bridge is now the live one. Store it + the fresh session so
    // subsequent reloads (or plugin calls still resolving the cells) see them.
    *ctx.action_bridge.lock().unwrap() = Some(Arc::clone(&fresh_bridge));
    *ctx.extension_session.lock().unwrap() = extension_session.clone();

    // ---- 3. resources_discover ("reload") over the fresh snapshot ----
    let discovered = extension_session
        .snapshot_arc()
        .map(|snap| rpi_extensions::emit_resources_discover(&cwd_str, "reload", &snap))
        .unwrap_or_default();

    // ---- 4. Re-run the Part-A loaders (same precedence + --no-* gates) ----
    let env = Arc::new(rpi_tools::OsExecutionEnv::with_cwd(ctx.cwd.clone()));
    let env_dyn: Arc<dyn rpi_tools::ExecutionEnv> = env.clone();

    let mut skills: Vec<rpi_harness::types::Skill> = Vec::new();
    let mut skill_diags: Vec<rpi_harness::skills::SkillDiagnostic> = Vec::new();
    if !ctx.args.no_skills {
        let mut dirs = skill_dirs(&ctx.cwd);
        dirs.extend(discovered.skill_paths.iter().map(PathBuf::from));
        dirs.extend(package_resources.skill_dirs());
        let result = load_skills_with_precedence(&env_dyn, &dirs).await;
        skills = result.skills;
        skill_diags = result.diagnostics;
    }

    let mut prompt_templates: Vec<rpi_harness::types::PromptTemplate> = Vec::new();
    let mut prompt_diags: Vec<rpi_harness::prompt_templates::PromptTemplateDiagnostic> = Vec::new();
    if !ctx.args.no_prompt_templates {
        let mut dirs = prompt_template_dirs(&ctx.cwd);
        dirs.extend(discovered.prompt_paths.iter().map(PathBuf::from));
        dirs.extend(package_resources.prompt_dirs());
        let result = load_prompt_templates_with_precedence(&env_dyn, &dirs).await;
        prompt_templates = result.prompt_templates;
        prompt_diags = result.diagnostics;
    }

    let context_block = if ctx.args.no_context_files {
        String::new()
    } else {
        let agent_dir = crate::config::agent_dir().ok();
        let agent_dir_path = agent_dir.unwrap_or_else(|| ctx.cwd.clone());
        let files = load_project_context_files(&env_dyn, &ctx.cwd, &agent_dir_path).await;
        format_project_context(&files)
    };

    if !skill_diags.is_empty()
        || !prompt_diags.is_empty()
        || !package_resources.diagnostics.is_empty()
    {
        warnings = true;
        if ctx.args.verbose {
            for d in &package_resources.diagnostics {
                eprintln!("warning: package {}: {}", d.spec, d.message);
            }
            for d in &skill_diags {
                eprintln!(
                    "warning: skill {} ({}): {}",
                    d.path,
                    d.code.as_str(),
                    d.message
                );
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
    }

    // ---- Re-compose the system prompt (same precedence as build) ----
    let base_prompt = match ctx.args.system_prompt.as_deref() {
        Some(explicit) => explicit.to_string(),
        None => match discover_system_prompt_file_with_packages(&ctx.cwd, &package_resources) {
            Some(path) => {
                std::fs::read_to_string(&path).unwrap_or_else(|_| default_system_prompt(&cwd_str))
            }
            None => default_system_prompt(&cwd_str),
        },
    };
    let mut append_texts: Vec<String> = Vec::new();
    for extra in &ctx.args.append_system_prompt {
        let text = read_append_target(extra).unwrap_or_else(|| extra.clone());
        append_texts.push(text);
    }
    if ctx.args.append_system_prompt.is_empty() {
        if let Some(path) =
            discover_append_system_prompt_file_with_packages(&ctx.cwd, &package_resources)
        {
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
    let system_prompt = compose_system_prompt(
        Some(&base_prompt),
        &[],
        if context_block.is_empty() {
            None
        } else {
            Some(&context_block)
        },
        append_join.as_deref(),
    );

    // ---- Rebuild the emitter (TeeEmitter over fresh ExtensionEmitter) ----
    let emitter: Arc<dyn rpi_agent::AgentEmitter> = match extension_session.snapshot_arc() {
        Some(snapshot) => {
            let ext = ExtensionEmitter::new(snapshot, extension_session.keepalive());
            Arc::new(TeeEmitter::new(vec![ctx.broadcast.clone(), Arc::new(ext)]))
        }
        None => ctx.broadcast.clone(),
    };

    // ---- 5. Push the rebuilt state into the live harness via the B5d setters ----
    let resources = AgentHarnessResources {
        skills: if skills.is_empty() {
            None
        } else {
            Some(skills.clone())
        },
        prompt_templates: if prompt_templates.is_empty() {
            None
        } else {
            Some(prompt_templates.clone())
        },
    };
    let _ = harness.set_system_prompt(Some(system_prompt)).await;
    let _ = harness.set_resources(resources).await;
    let _ = harness.set_agent_emitter(Some(emitter)).await;
    let _ = harness
        .set_models(build_models_with_extensions_for_reload(
            &ctx.gateway,
            &extension_session,
            ctx.runtime.clone(),
        ))
        .await;
    let _ = harness
        .set_provider_hooks(
            rpi_extensions::ExtensionProviderHooks::from_session(&extension_session)
                .map(|h| Arc::new(h) as Arc<dyn rpi_ai::ProviderHooks>),
        )
        .await;

    // Re-merge extension tools (a reloaded plugin may have added/removed a
    // tool). The built-in set is rebuilt from scratch + extension tools merged
    // on top, mirroring `build`.
    let mut_env: Arc<dyn rpi_tools::MutatingEnv> = env.clone();
    let tool_ctx = rpi_tools::ExecutionToolContext::new(env_dyn.clone(), Some(mut_env));
    let mut tools = build_tools(&tool_ctx, &ctx.args);
    merge_extension_tools(&mut tools, &extension_session, &ctx.args);
    let active = active_tool_names(&tools, &ctx.args);
    let _ = harness.set_tools(tools, Some(active)).await;

    let summary = format!(
        "Reloaded {} plugin(s), {} skill(s), {} prompt(s).",
        extension_session.loaded_paths().len(),
        skills.len(),
        prompt_templates.len(),
    );
    ReloadOutcome {
        summary,
        had_warnings: warnings,
    }
}

/// `build_models_with_extensions` for the reload path: the resolved gateway
/// (NOT `resolved` — the reload context carries the gateway `Arc<dyn Provider>`
/// directly, since the provider/auth did not change) first, then one
/// `PluggableProvider` per registered extension provider in the fresh session.
fn build_models_with_extensions_for_reload(
    gateway: &Arc<dyn Provider>,
    extension_session: &ExtensionSession,
    runtime: tokio::runtime::Handle,
) -> Vec<Arc<dyn Provider>> {
    let mut models: Vec<Arc<dyn Provider>> = vec![gateway.clone()];
    let pluggable = rpi_extensions::PluggableProvider::from_session(extension_session, runtime);
    models.extend(pluggable);
    models
}

/// Diagnostic for registered TUI renderers. All three renderer kinds are
/// consumed by the interactive TUI's JSON component adapter; this line remains
/// useful under `--verbose` for extension authors.
fn report_deferred_renderers(session: &ExtensionSession) {
    let Some(snap) = session.snapshot_arc() else {
        return;
    };
    let all = snap.renderers();
    let markdown = all
        .iter()
        .filter(|r| r.kind == rpi_extensions::RegisteredRendererKind::Markdown)
        .count();
    let message = all
        .iter()
        .filter(|r| r.kind == rpi_extensions::RegisteredRendererKind::Message)
        .count();
    let entry = all
        .iter()
        .filter(|r| r.kind == rpi_extensions::RegisteredRendererKind::Entry)
        .count();
    if markdown + message + entry == 0 {
        return;
    }
    eprintln!(
        "renderers: {} markdown-transform, {} message-render, {} entry-render (active)",
        markdown, message, entry
    );
}

/// A harness-build error.
#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("Could not create the session directory: {0}")]
    SessionDir(String),
    #[error("No session found for {requested} in {dir}. Start a fresh session instead (drop --continue/--resume/--session).")]
    SessionNotFound { requested: String, dir: String },
    #[error("Could not build the harness: {0}")]
    HarnessCreate(String),
}

/// B5c: build the `AgentHarnessOptions.models` vec — the resolved gateway
/// provider first, then one `Arc<dyn Provider>` per registered extension
/// provider (each a [`rpi_extensions::PluggableProvider`] wrapping a plugin's
/// sync `ProviderRequestFn`). The harness resolves a provider lazily per call by
/// `models.iter().find(|p| p.id() == model.provider)`, so the gateway stays
/// first-match for its own ids and an extension provider serves a catalog model
/// whose `provider` matches its id. `runtime` is the same `Handle` captured for
/// the action bridge — `PluggableProvider` needs a captured `Handle` to
/// `spawn_blocking` the sync ffi call from the async `stream_simple`.
fn build_models_with_extensions(
    resolved: &ResolvedModel,
    extension_session: &ExtensionSession,
    runtime: tokio::runtime::Handle,
) -> Vec<Arc<dyn Provider>> {
    let mut models: Vec<Arc<dyn Provider>> = vec![resolved.provider.clone() as Arc<dyn Provider>];
    let pluggable = rpi_extensions::PluggableProvider::from_session(extension_session, runtime);
    models.extend(pluggable);
    models
}

/// Resolve the extension dirs to scan and load the cdylib plugins, returning
/// the loaded session guard (keeps the `Library` handles alive for the harness
/// lifetime). Scan order: project `.rpi/extensions`, legacy `.pi/extensions`,
/// global `agent_dir()/`
/// `extensions`, then any `--extensions-dir` flags (scanned after the defaults
/// — `args.rs`). Diagnostics are a no-op sink for now; load skips/ABI mismatches
/// surface via the `--verbose` summary.
fn load_extensions(
    args: &Args,
    cwd: &Path,
    action_bridge: Option<Arc<rpi_extensions::ActionBridge>>,
) -> ExtensionSession {
    let mut dirs = project_dirs(cwd, EXTENSIONS_SUBDIR);
    if let Some(g) = global_dir(EXTENSIONS_SUBDIR) {
        dirs.push(g);
    }
    dirs.extend(args.extensions_dir.iter().cloned());
    let diagnostics: Arc<dyn PluginDiagnostics> = Arc::new(NullDiagnostics);
    // B5a: the action bridge is cloned into every loaded plugin's vtable
    // `user_data` so post-register `runtime_action` calls recover the harness
    // host from any thread. The call site already gates `load_extensions` behind
    // `!no_extensions` and threads `Some(bridge)`; `None` is only passed by the
    // `--no-extensions` branch (which calls `ExtensionSession::none()` directly)
    // and tests. Explicit `--extension`/`-e` files load after the dirs.
    rpi_extensions::load_session_mixed(&dirs, &args.extension, diagnostics, action_bridge)
}

fn js_extension_paths(
    args: &Args,
    cwd: &Path,
    packages: &crate::packages::PackageResources,
) -> Vec<PathBuf> {
    let mut paths = packages.extension_paths();
    for dir in project_dirs(cwd, EXTENSIONS_SUBDIR) {
        if let Ok(entries) = std::fs::read_dir(dir) {
            paths.extend(entries.flatten().map(|entry| entry.path()).filter(|path| {
                matches!(
                    path.extension()
                        .and_then(|ext| ext.to_str())
                        .map(|ext| ext.to_ascii_lowercase())
                        .as_deref(),
                    Some("js" | "mjs" | "cjs" | "ts" | "tsx")
                )
            }));
        }
    }
    paths.extend(
        args.extension
            .iter()
            .filter(|path| {
                matches!(
                    path.extension()
                        .and_then(|ext| ext.to_str())
                        .map(|ext| ext.to_ascii_lowercase())
                        .as_deref(),
                    Some("js" | "mjs" | "cjs" | "ts" | "tsx")
                )
            })
            .cloned(),
    );
    paths
}

/// Merge the loaded extension tools into the built-in set. An extension tool
/// overrides a same-named built-in; first-extension-wins across plugins is
/// already guaranteed by the registry (`register_tool` keeps the prior). The
/// explicit `--tools` allowlist / `--exclude-tools` denylist apply to the
/// merged set (the built-ins were already filtered in [`build_tools`]).
fn merge_extension_tools(tools: &mut Vec<HarnessTool>, session: &ExtensionSession, args: &Args) {
    let Some(snapshot) = session.snapshot() else {
        return;
    };
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

fn merge_js_extension_tools(
    tools: &mut Vec<HarnessTool>,
    session: &crate::js_extensions::JsExtensionSession,
    args: &Args,
) {
    for adapter in session.tools() {
        let name = adapter.schema().name.clone();
        if args
            .tools
            .as_ref()
            .is_some_and(|allow| !allow.iter().any(|value| value == &name))
            || args
                .exclude_tools
                .as_ref()
                .is_some_and(|deny| deny.iter().any(|value| value == &name))
        {
            continue;
        }
        let harness_tool = HarnessTool::new(Arc::new(adapter));
        match tools
            .iter_mut()
            .find(|tool| tool.tool.schema().name == name)
        {
            Some(slot) => *slot = harness_tool,
            None => tools.push(harness_tool),
        }
    }
}

/// Build the tool list per `--tools`/`--exclude-tools`/`--no-tools`/
/// `--no-builtin-tools`. Mirrors the TS `tools`/`excludeTools`/`noTools`
/// resolution in `createAgentSession`.
/// Default bash timeout: 120s when the model doesn't pass one (prevents a
/// forgotten `timeout` from hanging the run forever — the "卡住" report).
/// `RPI_BASH_TIMEOUT` overrides; a model-supplied timeout always wins.
pub fn bash_options() -> rpi_tools::tools::bash::BashToolOptions {
    use rpi_tools::tools::bash::BashToolOptions;
    let default = std::env::var("RPI_BASH_TIMEOUT")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(120.0);
    BashToolOptions {
        command_prefix: None,
        default_timeout: Some(default),
    }
}

fn build_tools(ctx: &ExecutionToolContext, args: &Args) -> Vec<HarnessTool> {
    if args.no_tools {
        return Vec::new();
    }
    // Construct every built-in once (cheap; the allowlist filters below).
    // Read-only search tools (grep/find/ls) take the same context and need no
    // mutation queue — they go through the `FileSystem` trait only.
    let mut all: Vec<(&'static str, HarnessTool)> = vec![
        ("read", HarnessTool::new(create_read_tool(ctx, None))),
        (
            "bash",
            HarnessTool::new(create_bash_tool(ctx, Some(bash_options()))),
        ),
        ("edit", HarnessTool::new(create_edit_tool(ctx))),
        ("write", HarnessTool::new(create_write_tool(ctx))),
        ("grep", HarnessTool::new(create_grep_tool(ctx, None))),
        ("find", HarnessTool::new(create_find_tool(ctx, None))),
        ("ls", HarnessTool::new(create_ls_tool(ctx, None))),
        ("powershell", HarnessTool::new(create_powershell_tool(ctx))),
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

    all.into_iter()
        .map(|(_, t)| t.with_replay(ToolReplay::Safe))
        .collect()
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
        return allow
            .iter()
            .filter(|a| names.iter().any(|n| n == *a))
            .cloned()
            .collect();
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
        SessionSelection::Latest
        | SessionSelection::ById { .. }
        | SessionSelection::ByExactId { .. } => restore_session(selection, cwd).await,
        SessionSelection::Fork { source } => fork_session_at_launch(source, cwd).await,
    }
}

/// Open an existing JSONL session for `Latest` / `ById`. Mirrors the TS
/// `SessionManager.resume`/`open` flow: list the session dir (newest-first),
/// match the request, then open the matched file and wrap it in a `Session`
/// facade. The restored transcript renders into the TUI at startup and the
/// harness continues appending to the same file.
async fn restore_session(selection: &SessionSelection, cwd: &str) -> Result<Session, BuildError> {
    // `list_typed` is newest-first; `Latest` takes the head, `ById` matches
    // the id exactly or by file-name containment (so `--session 01a02…` or a
    // partial id works, mirroring the TS id/path matching).
    match selection {
        SessionSelection::Latest => {
            let metas = list_session_metadata(cwd).await?;
            let Some(meta) = metas.first() else {
                return Err(BuildError::SessionNotFound {
                    requested: "the most recent session".to_string(),
                    dir: default_session_dir(Path::new(cwd)).display().to_string(),
                });
            };
            open_session(meta, cwd).await
        }
        SessionSelection::ById { id } => open_session_by_id(id, cwd).await.map_err(|e| match e {
            OpenError::NotFound { requested } => BuildError::SessionNotFound {
                requested,
                dir: default_session_dir(Path::new(cwd)).display().to_string(),
            },
            OpenError::Other(msg) => BuildError::SessionDir(msg),
        }),
        SessionSelection::ByExactId { id } => {
            // Exact id match only (pi `--session-id`): restore when the
            // session exists, else create a fresh one under the default dir.
            let metas = list_session_metadata(cwd).await?;
            if let Some(meta) = metas.iter().find(|m| m.id == *id) {
                return open_session(meta, cwd).await;
            }
            let dir = default_session_dir(Path::new(cwd));
            std::fs::create_dir_all(&dir)
                .map_err(|e| BuildError::SessionDir(format!("{}: {e}", dir.display())))?;
            create_jsonl_session_with_id(&dir, cwd, Some(id.clone()))
                .await
                .map_err(|e| BuildError::SessionDir(format!("{}: {e}", dir.display())))
        }
        _ => unreachable!("restore_session only called for Latest/ById/ByExactId"),
    }
}

/// `--fork <path|id>`: open the source session, fork it into a new JSONL
/// session (records the parent id), and start in the fork.
async fn fork_session_at_launch(source: &str, cwd: &str) -> Result<Session, BuildError> {
    use rpi_harness::session::jsonl::{
        JsonlSessionCreateOptions, JsonlSessionRepo, JsonlSessionRepoOptions,
    };
    use rpi_harness::session::types::{ForkOptions, SessionStorage};
    use rpi_tools::FileSystem;

    let dir = default_session_dir(Path::new(cwd));
    std::fs::create_dir_all(&dir)
        .map_err(|e| BuildError::SessionDir(format!("{}: {e}", dir.display())))?;
    let env = Arc::new(OsExecutionEnv::with_cwd(PathBuf::from(cwd)));
    let fs: Arc<dyn FileSystem> = env.clone();
    let repo = JsonlSessionRepo::with_env_cwd(JsonlSessionRepoOptions {
        fs: fs.clone(),
        sessions_root: dir.to_string_lossy().into_owned(),
        clock: Arc::new(SystemClock),
        ids: Arc::new(DefaultIdGenerator::new()),
    });
    let metas = repo
        .list_typed(&rpi_harness::session::jsonl::JsonlSessionListOptions::default())
        .await
        .map_err(|e| BuildError::SessionDir(format!("list sessions: {e}")))?;
    let source_meta = metas
        .iter()
        .find(|m| m.id == *source || m.path.contains(source) || source.contains(&m.id))
        .ok_or_else(|| BuildError::SessionNotFound {
            requested: format!("--fork {source}"),
            dir: dir.display().to_string(),
        })?;
    let fork_storage = repo
        .fork_typed(
            source_meta,
            &JsonlSessionCreateOptions {
                id: None,
                parent_session_id: Some(source_meta.id.clone()),
                cwd: cwd.to_string(),
                metadata: None,
            },
            &ForkOptions::default(),
        )
        .await
        .map_err(|e| BuildError::SessionDir(format!("fork {}: {e}", source_meta.path)))?;
    let storage_arc: Arc<dyn SessionStorage> = Arc::new(fork_storage);
    Ok(Session::new(storage_arc, None))
}

/// Errors from [`open_session_by_id`], split so the CLI can map them to
/// [`BuildError`] while the TUI can surface a friendlier note.
pub enum OpenError {
    /// No session matched the request.
    NotFound { requested: String },
    /// The match existed but could not be opened/parsed.
    Other(String),
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenError::NotFound { requested } => write!(f, "no session matches {requested}"),
            OpenError::Other(msg) => write!(f, "{msg}"),
        }
    }
}

/// List the JSONL session metadata under the default session dir, newest
/// first. Shared by startup restore and the TUI `/session` hot-switch.
pub async fn list_session_metadata(
    cwd: &str,
) -> Result<Vec<rpi_harness::session::jsonl::JsonlSessionMetadata>, BuildError> {
    use rpi_harness::session::jsonl::{
        JsonlSessionListOptions, JsonlSessionRepo, JsonlSessionRepoOptions,
    };
    use rpi_tools::FileSystem;

    let dir = default_session_dir(Path::new(cwd));
    let env = Arc::new(OsExecutionEnv::with_cwd(PathBuf::from(cwd)));
    let fs: Arc<dyn FileSystem> = env.clone();
    let repo = JsonlSessionRepo::with_env_cwd(JsonlSessionRepoOptions {
        fs: fs.clone(),
        sessions_root: dir.to_string_lossy().into_owned(),
        clock: Arc::new(SystemClock),
        ids: Arc::new(DefaultIdGenerator::new()),
    });
    repo.list_typed(&JsonlSessionListOptions::default())
        .await
        .map_err(|e| BuildError::SessionDir(format!("list sessions: {e}")))
}

/// Open a session whose id matches exactly or by file-name containment
/// (so `--session 01a02…` / a partial id / a full file name all work). The
/// TUI `/session` hot-switch calls this with the selector's item value.
pub async fn open_session_by_id(id: &str, cwd: &str) -> Result<Session, OpenError> {
    let metas = list_session_metadata(cwd)
        .await
        .map_err(|e| OpenError::Other(e.to_string()))?;
    let Some(meta) = metas
        .iter()
        .find(|m| m.id == id || m.path.contains(id) || id.contains(&m.id))
    else {
        return Err(OpenError::NotFound {
            requested: format!("session {id}"),
        });
    };
    open_session(meta, cwd)
        .await
        .map_err(|e| OpenError::Other(e.to_string()))
}

/// Fork the harness's current session into a new JSONL session (new id, parent
/// set to the source) and wrap it in a `Session`. Mirrors the TUI's
/// `fork_session` flow (`interactive_tui.rs`) — hoisted here so both the TUI
/// and the plugin `runtime_action(Fork)` host share one implementation.
/// Returns the new `Session` (NOT yet swapped onto the harness — the caller
/// does `harness.set_session(...)`).
pub(crate) async fn fork_session_storage(
    harness: &AgentHarness,
    cwd: &str,
) -> Result<Session, String> {
    use rpi_harness::session::jsonl::{JsonlSessionRepo, JsonlSessionRepoOptions};
    use rpi_tools::FileSystem;

    let dir = default_session_dir(Path::new(cwd));
    let env = Arc::new(OsExecutionEnv::with_cwd(PathBuf::from(cwd)));
    let fs: Arc<dyn FileSystem> = env.clone();
    let repo = JsonlSessionRepo::with_env_cwd(JsonlSessionRepoOptions {
        fs,
        sessions_root: dir.to_string_lossy().into_owned(),
        clock: Arc::new(SystemClock),
        ids: Arc::new(DefaultIdGenerator::new()),
    });
    // The fork needs the rich JSONL metadata (with the on-disk path); resolve
    // it from the session list by the current session's id.
    let id = harness.session().storage().metadata().id.clone();
    let metas = list_session_metadata(cwd)
        .await
        .map_err(|e| e.to_string())?;
    let Some(source) = metas.iter().find(|m| m.id == id) else {
        return Err(format!("current session {id} not found on disk"));
    };
    let fork_storage = repo
        .fork_typed(
            source,
            &rpi_harness::session::jsonl::JsonlSessionCreateOptions {
                id: None,
                parent_session_id: Some(source.id.clone()),
                cwd: cwd.to_string(),
                metadata: None,
            },
            &rpi_harness::session::types::ForkOptions::default(),
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok(Session::new(Arc::new(fork_storage), None))
}

/// Wrap an opened [`JsonlSessionStorage`] in the `Session` facade (shared by
/// startup restore + TUI hot-switch).
async fn open_session(
    meta: &rpi_harness::session::jsonl::JsonlSessionMetadata,
    cwd: &str,
) -> Result<Session, BuildError> {
    use rpi_harness::session::jsonl::{JsonlSessionRepo, JsonlSessionRepoOptions};
    use rpi_harness::session::types::SessionStorage;
    use rpi_tools::FileSystem;

    let dir = default_session_dir(Path::new(cwd));
    let env = Arc::new(OsExecutionEnv::with_cwd(PathBuf::from(cwd)));
    let fs: Arc<dyn FileSystem> = env.clone();
    let repo = JsonlSessionRepo::with_env_cwd(JsonlSessionRepoOptions {
        fs: fs.clone(),
        sessions_root: dir.to_string_lossy().into_owned(),
        clock: Arc::new(SystemClock),
        ids: Arc::new(DefaultIdGenerator::new()),
    });
    let storage = repo
        .open_by_jsonl_metadata(meta)
        .await
        .map_err(|e| BuildError::SessionDir(format!("open {}: {e}", meta.path)))?;
    let storage_arc: Arc<dyn SessionStorage> = Arc::new(storage);
    Ok(Session::new(storage_arc, None))
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
/// Create a fresh JSONL session file under `dir` and wrap it in a `Session`.
///
/// Uses the `JsonlSessionRepo` over an `OsExecutionEnv`-backed `FileSystem`
/// rooted at the cwd, so paths resolve consistently with the tools. Mirrors the
/// TS `SessionManager.create` flow (header write + `JsonlSessionStorage` open).
pub(crate) async fn create_jsonl_session(dir: &Path, cwd: &str) -> Result<Session, String> {
    create_jsonl_session_with_id(dir, cwd, None).await
}

/// `create_jsonl_session` with an explicit id (the `--session-id` fixed-id
/// contract: the file is named with the given id so later `--session-id`
/// launches restore the same session).
pub(crate) async fn create_jsonl_session_with_id(
    dir: &Path,
    cwd: &str,
    id: Option<String>,
) -> Result<Session, String> {
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
        id, // fresh uuidv7 when None (--session-id passes the fixed id)
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
        let args = Args {
            no_session: true,
            ..Args::default()
        };
        let cwd = Path::new("/tmp");
        assert!(matches!(
            select_session(&args, cwd),
            SessionSelection::Ephemeral
        ));
    }

    #[test]
    fn select_latest_for_continue_and_resume() {
        let args = Args {
            continue_session: true,
            ..Args::default()
        };
        let cwd = Path::new("/tmp");
        assert!(matches!(
            select_session(&args, cwd),
            SessionSelection::Latest
        ));

        let args = Args {
            resume: true,
            ..Args::default()
        };
        assert!(matches!(
            select_session(&args, cwd),
            SessionSelection::Latest
        ));
    }

    #[test]
    fn select_by_id_for_session_flag() {
        let args = Args {
            session: Some("01a02ece".into()),
            ..Args::default()
        };
        let cwd = Path::new("/tmp");
        assert!(matches!(
            select_session(&args, cwd),
            SessionSelection::ById { id } if id == "01a02ece"
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
                assert_eq!(dir, Path::new("/proj/.rpi/sessions"));
            }
            other => panic!("expected New, got {other:?}"),
        }
    }

    #[test]
    fn default_session_dir_prefers_rpi_but_reads_legacy_pi() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path();
        std::fs::create_dir_all(cwd.join(".pi/sessions")).unwrap();
        assert_eq!(default_session_dir(cwd), cwd.join(".pi/sessions"));
        std::fs::create_dir_all(cwd.join(".rpi/sessions")).unwrap();
        assert_eq!(default_session_dir(cwd), cwd.join(".rpi/sessions"));
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
