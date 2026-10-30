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
//! - `append_entry` → the harness's boundary-aware message writer, or raw
//!   extension data on the session; `set_session_name` → `set_name`.
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
//! `AgentHarness` directly — it holds an [`Arc<OnceLock<WeakAgentHarness>>`]
//! that is **empty at register time** and **filled once** by
//! [`HarnessActionHost::set_harness`] immediately after `AgentHarness::create`
//! succeeds. Each action upgrades that weak binding for its duration; the host
//! must not keep tools/providers alive via adapter -> keepalive -> bridge -> host.
//! Before construction or after final harness release, actions fail explicitly.
//! The bridge's opaque handle stays stable without retaining the harness.
//!
//! The impl needs the model `catalog` for `set_model(id)` (the lane wants a
//! `Model`, not an id) and the `cwd` for session create/fork/switch. Both are
//! held by `rpi-cli` at build time and moved into the host.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock, RwLock};

use rpi_ai::types::ThinkingLevel;
use rpi_extensions::RuntimeActionHost;
use rpi_harness::agent_harness::{
    AgentHarness, HarnessRunOutcome, NavigationOutcome, SendMessageResult, WeakAgentHarness,
};
use rpi_harness::session::session::Session;
use rpi_harness::types::SendMessageOptions;
use tokio::runtime::Handle;

use crate::session::{default_session_dir, open_session_by_id_in_dir};

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
    /// Weak binding filled after construction. Each operation upgrades it;
    /// before construction or after final owner release it returns an error.
    harness: Arc<OnceLock<WeakAgentHarness>>,
    /// The auth-filtered catalog (the same list the TUI `/model` selector
    /// shows). `set_model(id)` resolves an id against this.
    catalog: RwLock<Vec<rpi_ai::Model>>,
    /// The session cwd — `new_session`/`fork`/`switch_session` need it to
    /// locate the session dir.
    cwd: PathBuf,
    session_dir: PathBuf,
    /// Parsed unknown long flags retained for extension consumption.
    cli_flags: BTreeMap<String, serde_json::Value>,
    /// The session's plugin-tool context (`cwd` + session id). The session
    /// runtime actions (`new_session`/`fork`/`switch_session`) swap the session
    /// the harness serves, and must republish the id here too — a plugin whose
    /// tool state keyed off the old id would otherwise keep serving the session
    /// it named before the swap.
    tool_context: rpi_extensions::ToolCallContext,
    children: crate::child_agents::ChildAgents,
    side_questions: crate::side_questions::SideQuestions,
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
        cli_flags: BTreeMap<String, serde_json::Value>,
        tool_context: rpi_extensions::ToolCallContext,
    ) -> (Self, Arc<OnceLock<WeakAgentHarness>>) {
        let harness = Arc::new(OnceLock::new());
        (
            Self {
                harness: Arc::clone(&harness),
                catalog: RwLock::new(catalog),
                session_dir: default_session_dir(&cwd),
                cwd,
                cli_flags,
                tool_context,
                children: crate::child_agents::ChildAgents::default(),
                side_questions: crate::side_questions::SideQuestions::default(),
                runtime,
            },
            harness,
        )
    }

    pub fn set_catalog(&self, catalog: Vec<rpi_ai::Model>) {
        *self
            .catalog
            .write()
            .unwrap_or_else(|error| error.into_inner()) = catalog;
    }

    pub(crate) fn with_session_dir(mut self, session_dir: PathBuf) -> Self {
        self.session_dir = session_dir;
        self
    }

    /// Fill the harness cell. Call exactly once, immediately after
    /// `AgentHarness::create` succeeds. Returns the host (for fluent chaining)
    /// — the caller already holds the `Arc<dyn RuntimeActionHost>` from
    /// construction; this just populates the cell that host reads.
    pub fn set_harness(cell: &Arc<OnceLock<WeakAgentHarness>>, harness: Arc<AgentHarness>) {
        let _ = cell.set(harness.downgrade());
    }

    /// Pin the harness only for this action. The binding itself must not retain
    /// tools/providers through the plugin -> keepalive -> host reference cycle.
    fn harness(&self) -> Result<AgentHarness, String> {
        self.harness
            .get()
            .ok_or_else(|| "runtime action invoked before harness was built".to_string())?
            .upgrade()
            .ok_or_else(|| "runtime action invoked after harness was dropped".to_string())
    }

    /// Resolve `id` (case-insensitive exact, then substring) against the
    /// catalog. Mirrors the resolver's exact-id-first fallback.
    fn resolve_model(&self, id: &str) -> Option<rpi_ai::Model> {
        let catalog = self.catalog.read().ok()?;
        catalog
            .iter()
            .find(|m| m.id.eq_ignore_ascii_case(id))
            .cloned()
            .or_else(|| {
                catalog
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
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
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

/// Outcome of parsing `SendUserMessage`'s optional `images` array.
struct ParsedImages {
    /// The images that will actually be sent, in input order.
    images: Vec<rpi_ai::types::ImageContent>,
    /// How many entries were dropped (malformed, empty, or not an object).
    skipped: usize,
}

/// Parse `SendUserMessage`'s `images` array into [`rpi_ai::types::ImageContent`].
///
/// The wire shape matches pi's extension API — `{"mediaType":"image/png",
/// "data":"<base64>"}` — **not** `ImageContent`'s own serde shape, so this is a
/// hand-written mapping rather than `serde_json::from_value`. (`ImageContent`
/// serializes `mimeType`; the plugin-facing key is `mediaType`. Accepting both
/// keeps a plugin that already speaks the stored shape working.)
///
/// A bad image is **skipped, not fatal**: one malformed screenshot must not
/// prevent the message from being delivered. The caller reports the count so an
/// extension can surface "2 of 3 attached". An absent `images` key is not an
/// error (the overwhelming majority of sends are text-only); a present but
/// non-array `images` key is, because that is a caller bug rather than content.
fn parse_images(args: &serde_json::Value) -> Result<ParsedImages, String> {
    use base64::Engine as _;

    let Some(value) = args.get("images") else {
        return Ok(ParsedImages {
            images: Vec::new(),
            skipped: 0,
        });
    };
    if value.is_null() {
        return Ok(ParsedImages {
            images: Vec::new(),
            skipped: 0,
        });
    }
    let entries = value
        .as_array()
        .ok_or_else(|| "`images` must be an array of {mediaType, data} objects".to_string())?;

    let mut images = Vec::with_capacity(entries.len());
    let mut skipped = 0usize;
    for entry in entries {
        let Some(object) = entry.as_object() else {
            skipped += 1;
            continue;
        };
        // `mediaType` is the pi/extension-API spelling; `mimeType` is what the
        // stored `ImageContent` block uses. Accept both.
        let mime_type = object
            .get("mediaType")
            .or_else(|| object.get("mimeType"))
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let data = object
            .get("data")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        // Reject anything that is not actually an image, or that is not valid
        // base64. Whitespace (line-wrapped base64) is stripped first, matching
        // what a copy-pasted data URL looks like.
        let compact: String = data.chars().filter(|c| !c.is_whitespace()).collect();
        if !mime_type.starts_with("image/")
            || compact.is_empty()
            || base64::engine::general_purpose::STANDARD
                .decode(&compact)
                .is_err()
        {
            skipped += 1;
            continue;
        }
        images.push(rpi_ai::types::ImageContent {
            kind: rpi_ai::types::ImageContentType,
            // Send the normalized (unwrapped) payload: the compact form is what
            // every provider expects and what the stored block round-trips.
            data: compact,
            mime_type: mime_type.to_string(),
        });
    }
    Ok(ParsedImages { images, skipped })
}

/// Render a `HarnessRunOutcome` as JSON for the plugin. Only the terminal
/// status + leaf id cross (the final message text is folded to a string; full
/// assistant content is too rich for a v1 action result).
pub(crate) fn run_outcome_json(outcome: HarnessRunOutcome) -> serde_json::Value {
    match outcome {
        HarnessRunOutcome::Completed {
            leaf_id,
            final_entry_id,
            final_message,
        } => {
            serde_json::json!({
                "status": "completed",
                "leafId": leaf_id,
                "finalEntryId": final_entry_id,
                "text": assistant_text(&final_message),
            })
        }
        HarnessRunOutcome::Aborted {
            leaf_id,
            final_entry_id,
            final_message,
        } => {
            serde_json::json!({
                "status": "aborted",
                "leafId": leaf_id,
                "finalEntryId": final_entry_id,
                "text": assistant_text(&final_message),
            })
        }
        HarnessRunOutcome::Failed {
            leaf_id,
            error,
            final_entry_id,
            final_message,
        } => {
            serde_json::json!({
                "status": "failed",
                "leafId": leaf_id,
                "error": format!("{error:?}"),
                "finalEntryId": final_entry_id,
                "text": final_message.map(|m| assistant_text(&m)).unwrap_or_default(),
            })
        }
        HarnessRunOutcome::Suspended {
            leaf_id,
            final_entry_id,
            ..
        } => {
            serde_json::json!({
                "status": "suspended",
                "leafId": leaf_id,
                "finalEntryId": final_entry_id,
            })
        }
    }
}

/// Accept the SDK's flat action args and Pi-style nested options. Reject mixed
/// forms instead of silently choosing one set of scheduling instructions.
fn send_message_options(args: &serde_json::Value) -> Result<SendMessageOptions, String> {
    let flat = args.get("triggerTurn").is_some() || args.get("deliverAs").is_some();
    let value = if let Some(options) = args.get("options").filter(|value| !value.is_null()) {
        if !options.is_object() {
            return Err("send_message options must be an object".into());
        }
        if flat {
            return Err(
                "put triggerTurn/deliverAs either in options or at the top level, not both".into(),
            );
        }
        options.clone()
    } else {
        let mut fields = serde_json::Map::new();
        for key in ["triggerTurn", "deliverAs"] {
            if let Some(value) = args.get(key) {
                fields.insert(key.into(), value.clone());
            }
        }
        serde_json::Value::Object(fields)
    };
    serde_json::from_value(value).map_err(|error| format!("invalid send_message options: {error}"))
}

fn send_message_input(args: &serde_json::Value) -> Result<rpi_agent::AgentMessage, String> {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64;
    if let Some(message) = args.get("message") {
        if message.get("kind").is_some() {
            return serde_json::from_value(message.clone())
                .map_err(|error| format!("invalid message: {error}"));
        }
        // Pi's custom message input does not include the stored AgentMessage
        // discriminator. Preserve customType/display/details via its constructor.
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct CustomInput {
            custom_type: String,
            #[serde(default)]
            content: Option<rpi_ai::types::UserContent>,
            #[serde(default)]
            display: bool,
            #[serde(default)]
            details: Option<serde_json::Value>,
        }
        let input: CustomInput = serde_json::from_value(message.clone())
            .map_err(|error| format!("invalid custom message: {error}"))?;
        return Ok(rpi_harness::messages::create_custom_message(
            &input.custom_type,
            input
                .content
                .unwrap_or(rpi_ai::types::UserContent::Blocks(Vec::new())),
            input.display,
            input.details,
            timestamp,
        ));
    }
    let text =
        arg_str(args, "text").map_err(|_| "missing `message` or `text` field".to_string())?;
    Ok(rpi_agent::AgentMessage::User(
        rpi_ai::types::UserMessage::new(text, timestamp),
    ))
}

fn send_message_result(result: SendMessageResult) -> serde_json::Value {
    match result {
        SendMessageResult::Appended { entry_id } => {
            serde_json::json!({"status":"appended", "entryId":entry_id})
        }
        SendMessageResult::Queued { entry_id, queue } => {
            if queue == rpi_harness::session::types::QueueKind::Context {
                return serde_json::json!({"status":"queued", "entryId":entry_id, "queue":"context"});
            }
            let delivery = match queue {
                rpi_harness::session::types::QueueKind::NextRun => "nextTurn",
                _ => queue.as_str(),
            };
            serde_json::json!({"status":"queued", "entryId":entry_id, "deliverAs":delivery})
        }
        SendMessageResult::Run(run) => run_outcome_json(run.outcome),
    }
}

/// Extract the concatenated text from an assistant message (the `text` blocks).
pub(crate) fn assistant_text(msg: &rpi_ai::types::AssistantMessage) -> String {
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
    fn cancel_side_questions(&self, owner: u64) {
        self.side_questions.cancel_owner(owner);
    }
    fn side_question_revision(&self) -> u64 {
        self.side_questions.revision()
    }
    fn side_question_views(&self) -> Vec<serde_json::Value> {
        self.side_questions.views()
    }
    async fn side_question_ui(
        &self,
        owner: u64,
        thread_id: String,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        self.side_questions
            .ui_handle(&self.harness()?, owner, thread_id, args)
            .await
    }
    async fn side_question(
        &self,
        owner: u64,
        args: serde_json::Value,
        sink: rpi_extensions::SideQuestionSink,
    ) -> Result<serde_json::Value, String> {
        self.side_questions
            .handle(&self.harness()?, owner, args, sink)
            .await
    }

    fn cancel_child_agents(&self, owner: u64) {
        self.children.cancel_owner(owner);
    }
    fn replace_model_catalog(&self, models: Vec<rpi_ai::Model>) {
        self.set_catalog(models);
    }
    async fn get_execution_context(
        &self,
        _args: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let harness = self.harness()?;
        let mut context = harness.execution_context().map_err(|e| e.to_string())?;
        if let Some(serde_json::Value::Object(child)) =
            rpi_agent::tool_execution_scope::execution_context()
        {
            context
                .as_object_mut()
                .expect("execution context is an object")
                .extend(child);
        }
        context["cwd"] = serde_json::json!(self.cwd);
        context["sessionId"] = serde_json::json!(harness.session().storage().metadata().id);
        Ok(context)
    }

    async fn child_agent(
        &self,
        owner: u64,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        if rpi_agent::tool_execution_scope::is_child_execution() {
            return Err("recursive child agents are not allowed".into());
        }
        if args.get("op").and_then(serde_json::Value::as_str) == Some("start") {
            self.children.handle(&self.harness()?, owner, args).await
        } else {
            // Reclamation must remain possible after the parent was dropped.
            self.children.handle_existing(owner, args)
        }
    }

    async fn get_cli_flag(&self, args: serde_json::Value) -> Result<serde_json::Value, String> {
        let name = arg_str(&args, "name")?;
        Ok(serde_json::json!({
            "value": self.cli_flags.get(&name).cloned().unwrap_or(serde_json::Value::Null)
        }))
    }

    async fn send_message(&self, args: serde_json::Value) -> Result<serde_json::Value, String> {
        let options = send_message_options(&args)?;
        let message = send_message_input(&args)?;
        let lane = self.harness()?.lane("main");
        let result = lane
            .send_message(message, options)
            .await
            .map_err(|e| e.to_string())?;
        Ok(send_message_result(result))
    }

    async fn send_user_message(
        &self,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        // Text and images are independent: a pure-image message (a pasted
        // screenshot with no caption) is valid, so `text` is optional as long as
        // at least one image survives parsing. Requiring `text` first is what
        // made image-only sends impossible.
        let text = arg_str_opt(&args, "text").unwrap_or_default();
        let parsed = parse_images(&args)?;
        if text.is_empty() && parsed.images.is_empty() {
            return Err("send_user_message requires `text` or at least one image".to_string());
        }
        let sent = parsed.images.len();
        let lane = self.harness()?.lane("main");
        let result = lane
            .prompt_text(&text, parsed.images)
            .await
            .map_err(|e| e.to_string())?;
        let mut outcome = run_outcome_json(result.outcome);
        if let Some(object) = outcome.as_object_mut() {
            // `skippedImages` is always present so a caller can distinguish "no
            // images were skipped" from "this host does not report it".
            object.insert("skippedImages".into(), serde_json::json!(parsed.skipped));
            object.insert("images".into(), serde_json::json!(sent));
        }
        Ok(outcome)
    }

    async fn append_entry(&self, args: serde_json::Value) -> Result<serde_json::Value, String> {
        // `{"message": <AgentMessage json>}` appends a message entry; OR
        // `{"customType": "...", "data": {...}}` appends a custom entry. No run
        // is driven. Messages use the harness boundary writer, whose entry id
        // may be reserved until the current turn settles. Custom data stays out
        // of model context unless a caller explicitly configures a projector.
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
            .append_message(message)
            .await
            .map_err(|e| e.to_string())?;
        Ok(serde_json::json!({ "entryId": id }))
    }

    async fn get_session_branch(
        &self,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let request = crate::modes::branch_entry_query(&args)?;
        let entries = crate::modes::find_branch_entries(&self.harness()?, &request).await?;
        let entries = entries
            .iter()
            .map(serde_json::to_value)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("serialize session branch: {e}"))?;
        Ok(serde_json::json!({
            "entries": entries,
            "count": entries.len(),
            "filtered": true,
        }))
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

    async fn get_active_tools(
        &self,
        _args: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let lane = self.harness()?.lane("main");
        let tools = lane.get_active_tools().await.map_err(|e| e.to_string())?;
        Ok(serde_json::json!({ "tools": tools }))
    }

    async fn set_active_tools(&self, args: serde_json::Value) -> Result<serde_json::Value, String> {
        let tools = arg_str_array(&args, "tools")?;
        let lane = self.harness()?.lane("main");
        lane.set_active_tools(tools)
            .await
            .map_err(|e| e.to_string())?;
        Ok(serde_json::Value::Null)
    }

    async fn set_model(&self, args: serde_json::Value) -> Result<serde_json::Value, String> {
        let id = arg_str(&args, "model")?;
        let model = self
            .resolve_model(&id)
            .ok_or_else(|| format!("model `{id}` not in catalog"))?;
        let lane = self.harness()?.lane("main");
        lane.set_model(model.clone())
            .await
            .map_err(|e| e.to_string())?;
        Ok(serde_json::json!({ "model": model.id }))
    }

    async fn get_thinking_level(
        &self,
        _args: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let lane = self.harness()?.lane("main");
        let level = lane.get_thinking_level().await.map_err(|e| e.to_string())?;
        Ok(serde_json::json!({ "level": level }))
    }

    async fn set_thinking_level(
        &self,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
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
        lane.set_thinking_level(level)
            .await
            .map_err(|e| e.to_string())?;
        Ok(serde_json::Value::Null)
    }

    async fn compact(&self, args: serde_json::Value) -> Result<serde_json::Value, String> {
        let custom = arg_str_opt(&args, "customInstructions");
        let lane = self.harness()?.lane("main");
        let result = lane
            .compact(custom.as_deref())
            .await
            .map_err(|e| e.to_string())?;
        Ok(
            serde_json::json!({ "runId": result.run_id, "outcome": format!("{:?}", result.outcome) }),
        )
    }

    async fn get_system_prompt(
        &self,
        _args: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let prompt = self
            .harness()?
            .get_system_prompt()
            .await
            .map_err(|e| e.to_string())?;
        Ok(serde_json::json!({ "prompt": prompt }))
    }

    async fn new_session(&self, _args: serde_json::Value) -> Result<serde_json::Value, String> {
        let harness = self.harness()?;
        let cwd_str = self.cwd.to_string_lossy().to_string();
        let dir = &self.session_dir;
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("create session dir {}: {e}", dir.display()))?;
        let session = crate::session::create_jsonl_session(dir, &cwd_str)
            .await
            .map_err(|e| format!("create session: {e}"))?;
        let id = session.storage().metadata().id.clone();
        crate::session::set_active_session(&harness, session, Some(&self.tool_context)).await?;
        Ok(serde_json::json!({ "sessionId": id }))
    }

    async fn fork(&self, _args: serde_json::Value) -> Result<serde_json::Value, String> {
        let harness = self.harness()?;
        let cwd_str = self.cwd.to_string_lossy().to_string();
        let new_session =
            crate::session::fork_session_storage(&harness, &cwd_str, &self.session_dir)
                .await
                .map_err(|e| format!("fork session: {e}"))?;
        let id = new_session.storage().metadata().id.clone();
        crate::session::set_active_session(&harness, new_session, Some(&self.tool_context)).await?;
        Ok(serde_json::json!({ "sessionId": id }))
    }

    async fn navigate_tree(&self, args: serde_json::Value) -> Result<serde_json::Value, String> {
        let target_id = arg_str_opt(&args, "targetId");
        let summarize = arg_bool(&args, "summarize");
        let custom = arg_str_opt(&args, "customInstructions");
        let label = arg_str_opt(&args, "label");
        let lane = self.harness()?.lane("main");
        let result = lane
            .navigate_tree(
                target_id.as_deref(),
                summarize,
                custom.as_deref(),
                label.as_deref(),
            )
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
        let harness = self.harness()?;
        let id = arg_str(&args, "id")?;
        let cwd_str = self.cwd.to_string_lossy().to_string();
        let new_session: Session = open_session_by_id_in_dir(&id, &cwd_str, &self.session_dir)
            .await
            .map(|restored| restored.session)
            .map_err(|e| e.to_string())?;
        let new_id = new_session.storage().metadata().id.clone();
        crate::session::set_active_session(&harness, new_session, Some(&self.tool_context)).await?;
        Ok(serde_json::json!({ "sessionId": new_id }))
    }

    async fn reload(&self, _args: serde_json::Value) -> Result<serde_json::Value, String> {
        // Reached only when `ActionBridge.reload` is `None` (no /reload wired).
        // B5d wires the reload callback at the bridge layer; this host impl is
        // the "not configured" fallback.
        Err("reload not configured (no /reload callback on this bridge)".to_string())
    }
}

#[cfg(test)]
mod lifetime_tests;

#[cfg(test)]
mod send_user_message_tests {
    use super::*;
    use base64::Engine as _;
    use serde_json::json;

    /// 1x1 transparent PNG, base64. A real image keeps the test honest about
    /// what the provider actually receives.
    fn png_base64() -> String {
        base64::engine::general_purpose::STANDARD.encode([
            0x89u8, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1f, 0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0a, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9c, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00,
            0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
        ])
    }

    #[test]
    fn text_only_send_is_unchanged() {
        let parsed = parse_images(&json!({"text":"hello"})).unwrap();
        assert!(parsed.images.is_empty());
        assert_eq!(parsed.skipped, 0);
        // An explicit null / empty array is the same as omitting the key.
        for args in [json!({"text":"hi","images":null}), json!({"text":"hi","images":[]})] {
            let parsed = parse_images(&args).unwrap();
            assert!(parsed.images.is_empty(), "{args}");
            assert_eq!(parsed.skipped, 0);
        }
    }

    #[test]
    fn text_plus_images_maps_media_type_to_the_content_block() {
        let parsed = parse_images(&json!({
            "text":"look at this",
            "images":[{"mediaType":"image/png","data":png_base64()}]
        }))
        .unwrap();
        assert_eq!(parsed.skipped, 0);
        assert_eq!(
            parsed.images,
            vec![rpi_ai::types::ImageContent {
                kind: rpi_ai::types::ImageContentType,
                data: png_base64(),
                mime_type: "image/png".to_string(),
            }]
        );
        // The stored shape's `mimeType` spelling is accepted too.
        let stored_shape = parse_images(&json!({
            "images":[{"mimeType":"image/png","data":png_base64()}]
        }))
        .unwrap();
        assert_eq!(stored_shape.images, parsed.images);
    }

    #[test]
    fn image_only_send_is_allowed() {
        // No `text` key at all: a pasted screenshot with no caption.
        let parsed = parse_images(&json!({
            "images":[{"mediaType":"image/png","data":png_base64()}]
        }))
        .unwrap();
        assert_eq!(parsed.images.len(), 1);
        assert!(arg_str_opt(&json!({"images":[]}), "text").is_none());
    }

    #[test]
    fn bad_images_are_skipped_and_counted_not_fatal() {
        let parsed = parse_images(&json!({
            "text":"two of four",
            "images":[
                // Valid.
                {"mediaType":"image/png","data":png_base64()},
                // Invalid base64.
                {"mediaType":"image/png","data":"!!!not-base64!!"},
                // Empty payload.
                {"mediaType":"image/png","data":""},
                // Not an image at all.
                {"mediaType":"text/plain","data":png_base64()},
                // Missing mediaType entirely.
                {"data":png_base64()},
                // Valid, so the run still gets a usable attachment.
                {"mediaType":"image/jpeg","data":png_base64()},
            ]
        }))
        .unwrap();
        assert_eq!(parsed.images.len(), 2, "the two valid images survive");
        assert_eq!(parsed.skipped, 4);
        assert_eq!(parsed.images[1].mime_type, "image/jpeg");
    }

    #[test]
    fn line_wrapped_base64_is_normalized_instead_of_rejected() {
        let wrapped = png_base64()
            .as_bytes()
            .chunks(16)
            .map(|chunk| String::from_utf8_lossy(chunk).to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let parsed = parse_images(&json!({
            "text":"wrapped",
            "images":[{"mediaType":"image/png","data":wrapped}]
        }))
        .unwrap();
        assert_eq!(parsed.skipped, 0);
        assert_eq!(parsed.images[0].data, png_base64(), "newlines stripped");
    }

    #[test]
    fn a_non_array_images_field_is_a_caller_error() {
        // Malformed *content* is skipped; a malformed *request* is rejected so
        // the bug surfaces instead of silently sending text only.
        assert!(parse_images(&json!({"text":"hi","images":"oops"})).is_err());
        assert!(parse_images(&json!({"text":"hi","images":{}})).is_err());
    }

    #[test]
    fn every_message_needs_text_or_at_least_one_usable_image() {
        // Mirrors the guard in `send_user_message`: empty text and no surviving
        // image must not silently start a blank run.
        let is_sendable = |args: &serde_json::Value| {
            let text = arg_str_opt(args, "text").unwrap_or_default();
            let parsed = parse_images(args).unwrap();
            !(text.is_empty() && parsed.images.is_empty())
        };
        for args in [
            json!({}),
            json!({"text":""}),
            json!({"text":"","images":[]}),
            json!({"images":[{"mediaType":"image/png","data":"bad"}]}),
        ] {
            assert!(!is_sendable(&args), "{args} should have been rejected");
        }
        // ...and each of these is accepted.
        for args in [
            json!({"text":"hi"}),
            json!({"images":[{"mediaType":"image/png","data":png_base64()}]}),
        ] {
            assert!(is_sendable(&args), "{args} should have been accepted");
        }
    }
}

#[cfg(test)]
mod send_message_tests {
    use super::*;
    use rpi_harness::types::MessageDelivery;

    #[test]
    fn options_accept_flat_and_nested_forms_with_pi_defaults() {
        assert_eq!(
            send_message_options(&serde_json::json!({"text":"context"})).unwrap(),
            SendMessageOptions::default()
        );
        let expected = SendMessageOptions {
            trigger_turn: Some(false),
            deliver_as: MessageDelivery::FollowUp,
        };
        for args in [
            serde_json::json!({"text":"context", "triggerTurn":false, "deliverAs":"followUp"}),
            serde_json::json!({"text":"context", "options":{"triggerTurn":false, "deliverAs":"followUp"}}),
        ] {
            assert_eq!(send_message_options(&args).unwrap(), expected);
        }
    }

    #[test]
    fn options_reject_invalid_or_ambiguous_scheduling_instructions() {
        for args in [
            serde_json::json!({"triggerTurn":"false"}),
            serde_json::json!({"deliverAs":"follow-up"}),
            serde_json::json!({"options":{"typo":true}}),
            serde_json::json!({"options":[], "text":"context"}),
            serde_json::json!({"options":{"triggerTurn":true}, "triggerTurn":false}),
        ] {
            assert!(send_message_options(&args).is_err(), "{args}");
        }
    }

    #[test]
    fn pi_custom_input_preserves_content_and_display_metadata() {
        let message = send_message_input(&serde_json::json!({"message":{
            "customType":"goal", "content":"goal state", "display":true, "details":{"step":2}
        }}))
        .unwrap();
        let rpi_agent::AgentMessage::Custom(custom) = &message else {
            panic!("custom")
        };
        assert_eq!(
            custom.content,
            vec![rpi_ai::types::Content::text("goal state")]
        );
        let data = rpi_harness::messages::custom_data(&message).unwrap();
        assert_eq!(data.custom_type, "goal");
        assert!(data.display);
        assert_eq!(data.details, Some(serde_json::json!({"step":2})));
        // The existing stored AgentMessage input remains supported.
        let stored = serde_json::to_value(&message).unwrap();
        assert_eq!(
            send_message_input(&serde_json::json!({"message":stored})).unwrap(),
            message
        );
    }

    #[test]
    fn missing_custom_content_is_empty_and_queued_results_use_pi_delivery_names() {
        let message = send_message_input(
            &serde_json::json!({"message":{"customType":"goal", "content":null}}),
        )
        .unwrap();
        assert!(
            matches!(message, rpi_agent::AgentMessage::Custom(custom) if custom.content.is_empty())
        );
        assert_eq!(
            send_message_result(SendMessageResult::Queued {
                entry_id: "reserved".into(),
                queue: rpi_harness::session::types::QueueKind::NextRun,
            }),
            serde_json::json!({"status":"queued", "entryId":"reserved", "deliverAs":"nextTurn"})
        );
    }
}
