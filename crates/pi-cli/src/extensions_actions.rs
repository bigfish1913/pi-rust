//! B5a — the `RuntimeActionHost` impl over the harness, lived in `rpi-cli`
//! (NOT `rpi-harness`) so `rpi-extensions` stays a leaf in the crate DAG. The
//! trait is defined in `rpi-extensions` (JSON + primitives only); this is the
//! host side that bridges the 16 [`RuntimeActionId`] actions to the harness.
//!
//! 10 ops delegate to `harness.lane("main")` (the `AgentLane` surface backs
//! `prompt_message`/`prompt_text`/`get_active_tools`/`set_active_tools`/
//! `set_model`/`get_thinking_level`/`set_thinking_level`/`compact`/
//! `navigate_tree`). Run-ops serialize via `acquire_run`'s `active_run` guard —
//! a concurrent plugin invocation surfaces `lane_busy` as the harness error
//! string, the right behavior (a plugin can't re-enter an active run).
//!
//! 6 non-lane ops:
//! - `append_entry`/`set_session_name` → `harness.session().append_message`/
//!   `set_name`.
//! - `get_system_prompt` → `AgentHarness::get_system_prompt` (B5a accessor).
//! - `new_session`/`fork`/`switch_session` → reuse `crate::session`'s
//!   create/fork/open helpers + `harness.set_session`.
//! - `reload` → handled by `ActionBridge.reload` (B5d); the host impl is the
//!   "not configured" fallback for bridges without a callback.
//!
//! ## Construction ordering (the load-bearing wrinkle)
//!
//! Extensions load **before** the harness is created (extensions provide the
//! tools the harness is built with), but the plugin stores the
//! `ActionBridge`'s `user_data` pointer during `register` and that pointer
//! must remain valid for the whole session. So the host cannot hold the
//! `AgentHarness` directly — it holds an [`Arc<OnceLock<Arc<AgentHarness>>>`]
//! that is **empty at register time** and **filled once** by
//! [`HarnessActionHost::set_harness`] immediately after `AgentHarness::create`
//! succeeds. No plugin can call a runtime action before the harness runs, so
//! the cell is always set before the first `get()`. The `Arc<dyn
//! RuntimeActionHost>` (and thus the `ActionBridge` pointer) is stable from
//! construction, satisfying the FFI lifetime requirement.
//!
//! The impl needs the model `catalog` for `set_model(id)` (the lane wants a
//! `Model`, not an id) and the `cwd` for session create/fork/switch. Both are
//! held by `rpi-cli` at build time and moved into the host.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use rpi_ai::types::ThinkingLevel;
use rpi_extensions::RuntimeActionHost;
use rpi_harness::agent_harness::{AgentHarness, HarnessRunOutcome, NavigationOutcome};
use rpi_harness::session::session::Session;
use tokio::runtime::Handle;

use crate::session::{default_session_dir, open_session_by_id};

/// The `RuntimeActionHost` impl over an `AgentHarness`. Built once per session
/// in [`crate::session::build`] — constructed **empty** before extension load
/// (the harness doesn't exist yet), then filled via [`set_harness`](Self::set_harness)
/// once `AgentHarness::create` succeeds. Carried inside the [`ActionBridge`] as
/// `Arc<dyn RuntimeActionHost>`.
///
/// `runtime` is captured at build time so the bridge can spawn dispatch from
/// any thread; the impl's async methods run ON that runtime (they are spawned
/// by the trampoline), so they may freely await.
pub struct HarnessActionHost {
    /// Filled by `set_harness` after the harness exists. `get()` is infallible
    /// once set; before that (only possible mid-build, before any plugin call)
    /// methods return a "not ready" error.
    harness: Arc<OnceLock<Arc<AgentHarness>>>,
    /// The auth-filtered catalog (the same list the TUI `/model` selector
    /// shows). `set_model(id)` resolves an id against this.
    catalog: Vec<rpi_ai::Model>,
    /// The session cwd — `new_session`/`fork`/`switch_session` need it to
    /// locate the session dir.
    cwd: PathBuf,
    #[allow(dead_code)]
    runtime: Handle,
}

impl HarnessActionHost {
    /// Build an **empty** host (no harness yet). The `runtime` is the handle
    /// the bridge captured (kept only so the impl can name it for future
    /// direct-spawn needs; the trampoline already spawns on the bridge's
    /// runtime). Call [`set_harness`](Self::set_harness) once the harness is
    /// created. Returns `(host, harness_cell)` where `harness_cell` is the
    /// shared `OnceLock` the caller fills.
    pub fn new_empty(
        catalog: Vec<rpi_ai::Model>,
        cwd: PathBuf,
        runtime: Handle,
    ) -> (Self, Arc<OnceLock<Arc<AgentHarness>>>) {
        let harness = Arc::new(OnceLock::new());
        (
            Self { harness: Arc::clone(&harness), catalog, cwd, runtime },
            harness,
        )
    }

    /// Fill the harness cell. Call exactly once, immediately after
    /// `AgentHarness::create` succeeds. Returns the host (for fluent chaining)
    /// — the caller already holds the `Arc<dyn RuntimeActionHost>` from
    /// construction; this just populates the cell that host reads.
    pub fn set_harness(cell: &Arc<OnceLock<Arc<AgentHarness>>>, harness: Arc<AgentHarness>) {
        // `set` panics if already set; that's the right failure (double-build
        // is a programming error, not a runtime condition).
        let _ = cell.set(harness);
    }

    /// Borrow the harness, or return a "not ready" error. Only reachable
    /// mid-build before `set_harness`; once the harness runs, plugins can fire
    /// actions and the cell is set.
    fn harness(&self) -> Result<&AgentHarness, String> {
        self.harness
            .get()
            .map(|h| h.as_ref())
            .ok_or_else(|| "runtime action invoked before harness was built".to_string())
    }

    /// Resolve `id` (case-insensitive exact, then substring) against the
    /// catalog. Mirrors the resolver's exact-id-first fallback.
    fn resolve_model(&self, id: &str) -> Option<rpi_ai::Model> {
        self.catalog
            .iter()
            .find(|m| m.id.eq_ignore_ascii_case(id))
            .cloned()
            .or_else(|| {
                self.catalog
                    .iter()
                    .find(|m| m.id.to_ascii_lowercase().contains(&id.to_ascii_lowercase()))
                    .cloned()
            })
    }
}

/// Helper: pull a string field `key` from `args` (object), or return `msg`.
fn arg_str(args: &serde_json::Value, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| format!("missing string field `{key}` in action args"))
}

/// Helper: pull an optional string field.
fn arg_str_opt(args: &serde_json::Value, key: &str) -> Option<String> {
    args.get(key).and_then(|v| v.as_str()).map(|s| s.to_string())
}

/// Helper: pull a bool field (default `false`).
fn arg_bool(args: &serde_json::Value, key: &str) -> bool {
    args.get(key).and_then(|v| v.as_bool()).unwrap_or(false)
}

/// Helper: pull a string array field.
fn arg_str_array(args: &serde_json::Value, key: &str) -> Result<Vec<String>, String> {
    args.get(key)
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .ok_or_else(|| format!("missing string-array field `{key}` in action args"))
}

/// Render a `HarnessRunOutcome` as JSON for the plugin. Only the terminal
/// status + leaf id cross (the final message text is folded to a string; full
/// assistant content is too rich for a v1 action result).
fn run_outcome_json(outcome: HarnessRunOutcome) -> serde_json::Value {
    match outcome {
        HarnessRunOutcome::Completed { leaf_id, final_entry_id, final_message } => {
            serde_json::json!({
                "status": "completed",
                "leafId": leaf_id,
                "finalEntryId": final_entry_id,
                "text": assistant_text(&final_message),
            })
        }
        HarnessRunOutcome::Aborted { leaf_id, final_entry_id, final_message } => {
            serde_json::json!({
                "status": "aborted",
                "leafId": leaf_id,
                "finalEntryId": final_entry_id,
                "text": assistant_text(&final_message),
            })
        }
        HarnessRunOutcome::Failed { leaf_id, error, final_entry_id, final_message } => {
            serde_json::json!({
                "status": "failed",
                "leafId": leaf_id,
                "error": format!("{error:?}"),
                "finalEntryId": final_entry_id,
                "text": final_message.map(|m| assistant_text(&m)).unwrap_or_default(),
            })
        }
        HarnessRunOutcome::Suspended { leaf_id, final_entry_id, .. } => {
            serde_json::json!({
                "status": "suspended",
                "leafId": leaf_id,
                "finalEntryId": final_entry_id,
            })
        }
    }
}

/// Extract the concatenated text from an assistant message (the `text` blocks).
fn assistant_text(msg: &rpi_ai::types::AssistantMessage) -> String {
    msg.content
        .iter()
        .filter_map(|b| match b {
            rpi_ai::types::Content::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

#[async_trait::async_trait]
impl RuntimeActionHost for HarnessActionHost {
    async fn send_message(&self, args: serde_json::Value) -> Result<serde_json::Value, String> {
        // `{"message": <AgentMessage json>}` — drive a full run from any message
        // kind. Falls back to `{"text": "..."}` as a user-text shorthand.
        let lane = self.harness()?.lane("main");
        if let Some(text) = arg_str_opt(&args, "text") {
            let result = lane.prompt_text(&text, Vec::new()).await.map_err(|e| e.to_string())?;
            return Ok(run_outcome_json(result.outcome));
        }
        let msg = args
            .get("message")
            .ok_or_else(|| "missing `message` or `text` field".to_string())?;
        let message: rpi_agent::AgentMessage =
            serde_json::from_value(msg.clone()).map_err(|e| format!("invalid message: {e}"))?;
        let result = lane.prompt_message(message).await.map_err(|e| e.to_string())?;
        Ok(run_outcome_json(result.outcome))
    }

    async fn send_user_message(&self, args: serde_json::Value) -> Result<serde_json::Value, String> {
        let text = arg_str(&args, "text")?;
        let lane = self.harness()?.lane("main");
        let result = lane.prompt_text(&text, Vec::new()).await.map_err(|e| e.to_string())?;
        Ok(run_outcome_json(result.outcome))
    }

    async fn append_entry(&self, args: serde_json::Value) -> Result<serde_json::Value, String> {
        // `{"message": <AgentMessage json>}` appends a message entry; OR
        // `{"customType": "...", "data": {...}}` appends a custom entry. No run
        // is driven — the entry lands in the transcript only.
        if let Some(custom_type) = arg_str_opt(&args, "customType") {
            let data = args.get("data").cloned();
            let id = self
                .harness()?
                .session()
                .append_custom_entry(&custom_type, data)
                .await
                .map_err(|e| e.to_string())?;
            return Ok(serde_json::json!({ "entryId": id }));
        }
        let msg = args
            .get("message")
            .ok_or_else(|| "missing `message` or `customType` field".to_string())?;
        let message: rpi_agent::AgentMessage =
            serde_json::from_value(msg.clone()).map_err(|e| format!("invalid message: {e}"))?;
        let id = self
            .harness()?
            .session()
            .append_message(message)
            .await
            .map_err(|e| e.to_string())?;
        Ok(serde_json::json!({ "entryId": id }))
    }

    async fn set_session_name(&self, args: serde_json::Value) -> Result<serde_json::Value, String> {
        let name = arg_str(&args, "name")?;
        self.harness()?
            .session()
            .set_name(Some(&name))
            .await
            .map_err(|e| e.to_string())?;
        Ok(serde_json::Value::Null)
    }

    async fn get_active_tools(&self, _args: serde_json::Value) -> Result<serde_json::Value, String> {
        let lane = self.harness()?.lane("main");
        let tools = lane.get_active_tools().await.map_err(|e| e.to_string())?;
        Ok(serde_json::json!({ "tools": tools }))
    }

    async fn set_active_tools(&self, args: serde_json::Value) -> Result<serde_json::Value, String> {
        let tools = arg_str_array(&args, "tools")?;
        let lane = self.harness()?.lane("main");
        lane.set_active_tools(tools).await.map_err(|e| e.to_string())?;
        Ok(serde_json::Value::Null)
    }

    async fn set_model(&self, args: serde_json::Value) -> Result<serde_json::Value, String> {
        let id = arg_str(&args, "model")?;
        let model = self
            .resolve_model(&id)
            .ok_or_else(|| format!("model `{id}` not in catalog"))?;
        let lane = self.harness()?.lane("main");
        lane.set_model(model.clone()).await.map_err(|e| e.to_string())?;
        Ok(serde_json::json!({ "model": model.id }))
    }

    async fn get_thinking_level(&self, _args: serde_json::Value) -> Result<serde_json::Value, String> {
        let lane = self.harness()?.lane("main");
        let level = lane.get_thinking_level().await.map_err(|e| e.to_string())?;
        Ok(serde_json::json!({ "level": level }))
    }

    async fn set_thinking_level(&self, args: serde_json::Value) -> Result<serde_json::Value, String> {
        let level_val = args
            .get("level")
            .ok_or_else(|| "missing `level` field".to_string())?;
        let level: ThinkingLevel = if let Some(s) = level_val.as_str() {
            serde_json::from_value(serde_json::Value::String(s.to_string()))
                .map_err(|e| format!("invalid thinking level `{s}`: {e}"))?
        } else {
            serde_json::from_value(level_val.clone())
                .map_err(|e| format!("invalid thinking level: {e}"))?
        };
        let lane = self.harness()?.lane("main");
        lane.set_thinking_level(level).await.map_err(|e| e.to_string())?;
        Ok(serde_json::Value::Null)
    }

    async fn compact(&self, args: serde_json::Value) -> Result<serde_json::Value, String> {
        let custom = arg_str_opt(&args, "customInstructions");
        let lane = self.harness()?.lane("main");
        let result = lane
            .compact(custom.as_deref())
            .await
            .map_err(|e| e.to_string())?;
        Ok(serde_json::json!({ "runId": result.run_id, "outcome": format!("{:?}", result.outcome) }))
    }

    async fn get_system_prompt(&self, _args: serde_json::Value) -> Result<serde_json::Value, String> {
        let prompt = self.harness()?
            .get_system_prompt()
            .await
            .map_err(|e| e.to_string())?;
        Ok(serde_json::json!({ "prompt": prompt }))
    }

    async fn new_session(&self, _args: serde_json::Value) -> Result<serde_json::Value, String> {
        let cwd_str = self.cwd.to_string_lossy().to_string();
        let dir = default_session_dir(&self.cwd);
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("create session dir {}: {e}", dir.display()))?;
        let session = crate::session::create_jsonl_session(&dir, &cwd_str)
            .await
            .map_err(|e| format!("create session: {e}"))?;
        let id = session.storage().metadata().id.clone();
        self.harness()?
            .set_session(session)
            .await
            .map_err(|e| e.to_string())?;
        Ok(serde_json::json!({ "sessionId": id }))
    }

    async fn fork(&self, _args: serde_json::Value) -> Result<serde_json::Value, String> {
        let cwd_str = self.cwd.to_string_lossy().to_string();
        let new_session = crate::session::fork_session_storage(self.harness()?, &cwd_str)
            .await
            .map_err(|e| format!("fork session: {e}"))?;
        let id = new_session.storage().metadata().id.clone();
        self.harness()?
            .set_session(new_session)
            .await
            .map_err(|e| e.to_string())?;
        Ok(serde_json::json!({ "sessionId": id }))
    }

    async fn navigate_tree(&self, args: serde_json::Value) -> Result<serde_json::Value, String> {
        let target_id = arg_str_opt(&args, "targetId");
        let summarize = arg_bool(&args, "summarize");
        let custom = arg_str_opt(&args, "customInstructions");
        let label = arg_str_opt(&args, "label");
        let lane = self.harness()?.lane("main");
        let result = lane
            .navigate_tree(target_id.as_deref(), summarize, custom.as_deref(), label.as_deref())
            .await
            .map_err(|e| e.to_string())?;
        let status = match &result.outcome {
            NavigationOutcome::Completed { .. } => "completed",
            NavigationOutcome::Declined { .. } => "declined",
            NavigationOutcome::Aborted { .. } => "aborted",
            NavigationOutcome::Failed { .. } => "failed",
        };
        Ok(serde_json::json!({ "runId": result.run_id, "status": status }))
    }

    async fn switch_session(&self, args: serde_json::Value) -> Result<serde_json::Value, String> {
        let id = arg_str(&args, "id")?;
        let cwd_str = self.cwd.to_string_lossy().to_string();
        let new_session: Session =
            open_session_by_id(&id, &cwd_str).await.map_err(|e| e.to_string())?;
        let new_id = new_session.storage().metadata().id.clone();
        self.harness()?
            .set_session(new_session)
            .await
            .map_err(|e| e.to_string())?;
        Ok(serde_json::json!({ "sessionId": new_id }))
    }

    async fn reload(&self, _args: serde_json::Value) -> Result<serde_json::Value, String> {
        // Reached only when `ActionBridge.reload` is `None` (no /reload wired).
        // B5d wires the reload callback at the bridge layer; this host impl is
        // the "not configured" fallback.
        Err("reload not configured (no /reload callback on this bridge)".to_string())
    }
}
