//! Branch-owned pre-skills configuration; dynamic skill composition stays in the harness.
use rpi_harness::agent_harness::AgentHarness;
use rpi_harness::session::types::{BranchBounds, Entry, EntryOrder, EntryQuery};

pub(crate) const MAX_SYSTEM_PROMPT_BYTES: usize = 256 * 1024;
pub(crate) const SYSTEM_PROMPT_ENTRY: &str = "rpi_system_prompt";

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct PromptSnapshot {
    pub version: u32,
    pub original_base: Option<String>,
    // v1 cannot recover the original base. Never guess by stripping a suffix.
    pub base_known: bool,
    pub client_prompt: String,
    pub mode: String,
    pub effective: String,
}

impl PromptSnapshot {
    pub fn configure(
        previous: Option<&Self>, base: Option<String>, prompt: &str, mode: &str,
    ) -> Result<Self, String> {
        validate_prompt(prompt)?;
        if !matches!(mode, "append" | "replace") {
            return Err("mode must be append or replace".into());
        }
        let original_base = previous.map_or(base, |p| p.original_base.clone());
        let base_known = previous.is_none_or(|p| p.base_known);
        if mode == "append" && !base_known {
            return Err("legacy system prompt has no recoverable native base; use replace or a new session".into());
        }
        let effective = if mode == "replace" {
            prompt.to_owned()
        } else {
            match (original_base.as_deref().filter(|s| !s.is_empty()), prompt.is_empty()) {
                (Some(base), false) => format!("{base}\n\n{prompt}"),
                (Some(base), true) => base.to_owned(),
                (None, _) => prompt.to_owned(),
            }
        };
        validate_prompt(&effective)?;
        Ok(Self { version: 2, original_base, base_known, client_prompt: prompt.into(), mode: mode.into(), effective })
    }
}

pub(crate) async fn persisted_snapshot(harness: &AgentHarness) -> Result<Option<PromptSnapshot>, String> {
    let entries = harness.find_entries_on_branch(
        &EntryQuery { custom_type: Some(SYSTEM_PROMPT_ENTRY.into()), order: Some(EntryOrder::NewestFirst), limit: Some(1), ..Default::default() },
        &BranchBounds::default(),
    ).await.map_err(|error| error.to_string())?;
    let Some(Entry::Custom(entry)) = entries.into_iter().next() else { return Ok(None); };
    let data = entry.data.ok_or("missing system prompt snapshot data")?;
    let snapshot = match data.get("version").and_then(serde_json::Value::as_u64) {
        Some(1) => PromptSnapshot {
            version: 1, original_base: None, base_known: false, client_prompt: String::new(), mode: String::new(),
            effective: data.get("prompt").and_then(serde_json::Value::as_str).ok_or("invalid legacy system prompt snapshot")?.into(),
        },
        Some(2) => {
            let snapshot: PromptSnapshot = serde_json::from_value(data).map_err(|error| error.to_string())?;
            if !snapshot.base_known && snapshot.original_base.is_some() {
                return Err("invalid unknown system prompt base".into());
            }
            let expected = PromptSnapshot::configure(Some(&snapshot), None, &snapshot.client_prompt, &snapshot.mode)?;
            if expected.effective != snapshot.effective { return Err("inconsistent system prompt snapshot".into()); }
            snapshot
        }
        _ => return Err("unsupported system prompt snapshot version".into()),
    };
    if let Some(base) = &snapshot.original_base { validate_prompt(base)?; }
    validate_prompt(&snapshot.effective)?;
    Ok(Some(snapshot))
}

pub(crate) async fn persisted_system_prompt(harness: &AgentHarness) -> Result<Option<String>, String> {
    Ok(persisted_snapshot(harness).await?.map(|snapshot| snapshot.effective))
}

pub(crate) fn validate_prompt(prompt: &str) -> Result<(), String> {
    if prompt.len() > MAX_SYSTEM_PROMPT_BYTES {
        return Err(format!("system prompt exceeds {MAX_SYSTEM_PROMPT_BYTES} UTF-8 bytes"));
    }
    Ok(())
}

pub(crate) async fn restore_system_prompt(harness: &AgentHarness) -> Result<(), String> {
    if let Some(prompt) = persisted_system_prompt(harness).await? {
        harness.set_system_prompt(Some(prompt)).await.map_err(|error| error.to_string())?;
    }
    Ok(())
}
