//! Mirrors `packages/agent/src/harness/session/context.ts` — building the
//! provider-facing [`SessionContext`] (messages + derived state) from a branch's
//! entry path.
//!
//! The authoritative port of `context.ts`. `compaction/tokens.rs` held a minimal
//! no-options copy pulled forward for `tokensBefore`; that copy now delegates
//! here (`build_session_context`) so the two never diverge.
//!
//! Shape notes:
//! - `defaultContextEntryTransform` keeps the **last** `compaction` entry plus
//!   everything after it; with no compaction, keeps all entries. This is the
//!   "resume from the latest summary" boundary the loop feeds the model.
//! - `sessionEntryToContextMessages` projects each entry to zero or more
//!   [`AgentMessage`]s: messages pass through (deferred assistant handles are
//!   dropped — they are placeholders, not model-visible content); a compaction
//!   entry expands to its `compactionSummary` message plus the retained tail; a
//!   non-empty `branch_summary` expands to a `branchSummary` message; a `custom`
//!   entry expands via the registered projector (or nothing).
//! - `deriveSessionContextState` walks the FULL path (not the truncated context
//!   entries) so `thinking_level`/`model`/`active_tool_names` reflect the latest
//!   change even when it fell before a compaction boundary.

use std::collections::BTreeMap;
use std::sync::Arc;

use pi_ai::types::StopReason;
use pi_agent::message::AgentMessage;

use crate::messages::{create_branch_summary_message, create_compaction_summary_message};
use crate::session::types::{BranchSummaryEntry, CompactionEntry, CustomEntry, Entry};

/// `SessionContext` — the provider-facing snapshot built from a branch path.
/// Mirrors TS `SessionContext`. `model` carries `(provider, model_id)`.
#[derive(Debug, Clone, Default)]
pub struct SessionContext {
    pub messages: Vec<AgentMessage>,
    pub thinking_level: String,
    pub model: Option<(String, String)>,
    pub active_tool_names: Option<Vec<String>>,
}

/// `ContextEntryTransform` — a caller-supplied transform applied to the context
/// entries AFTER the default compaction-boundary transform. Mirrors TS
/// `ContextEntryTransform`. `Arc` so it can live in an options bag.
pub type ContextEntryTransform = Arc<dyn Fn(&[Entry]) -> Vec<Entry> + Send + Sync>;

/// `CustomEntryContextMessageProjector` — projects a `custom` entry to context
/// messages. Mirrors TS `CustomEntryContextMessageProjector`. Receives
/// `(entry, index, entries)` for parity with the TS signature (most projectors
/// read only `entry`). Returns the messages to splice in (empty = omit).
pub type CustomEntryContextMessageProjector =
    Arc<dyn Fn(&CustomEntry, usize, &[Entry]) -> Vec<AgentMessage> + Send + Sync>;

/// `SessionContextBuildOptions`. Mirrors TS `SessionContextBuildOptions`.
#[derive(Clone, Default)]
pub struct SessionContextBuildOptions {
    pub entry_transforms: Vec<ContextEntryTransform>,
    pub entry_projectors: BTreeMap<String, CustomEntryContextMessageProjector>,
}

impl std::fmt::Debug for SessionContextBuildOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionContextBuildOptions")
            .field("entry_transforms", &self.entry_transforms.len())
            .field("entry_projectors", &self.entry_projectors.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// `defaultContextEntryTransform` — keep the last `compaction` entry + everything
/// after it; if no compaction, keep all entries. Mirrors
/// `defaultContextEntryTransform`.
pub fn default_context_entry_transform(path_entries: &[Entry]) -> Vec<Entry> {
    let mut compaction_index = None;
    for (i, entry) in path_entries.iter().enumerate().rev() {
        if matches!(entry, Entry::Compaction(_)) {
            compaction_index = Some(i);
            break;
        }
    }
    match compaction_index {
        None => path_entries.to_vec(),
        Some(i) => {
            let mut out = Vec::with_capacity(path_entries.len() - i);
            out.push(path_entries[i].clone());
            out.extend(path_entries[i + 1..].iter().cloned());
            out
        }
    }
}

/// `buildContextEntries` — apply the default transform, then each caller
/// transform in order. Mirrors TS `buildContextEntries`.
pub fn build_context_entries(path_entries: &[Entry], options: &SessionContextBuildOptions) -> Vec<Entry> {
    let mut entries = default_context_entry_transform(path_entries);
    for transform in &options.entry_transforms {
        entries = transform(&entries);
    }
    entries
}

/// `sessionEntryToContextMessages` — project one entry to context messages.
/// Mirrors TS `sessionEntryToContextMessages`.
pub fn session_entry_to_context_messages(
    entry: &Entry,
    index: usize,
    entries: &[Entry],
    options: &SessionContextBuildOptions,
) -> Vec<AgentMessage> {
    match entry {
        Entry::Message(m) => {
            if let AgentMessage::Assistant(a) = &m.message {
                if a.stop_reason == StopReason::Deferred {
                    return Vec::new();
                }
            }
            vec![m.message.clone()]
        }
        Entry::Compaction(c) => compaction_to_context_messages(c),
        Entry::BranchSummary(b) => branch_summary_to_context_messages(b),
        Entry::Custom(c) => custom_to_context_messages(c, index, entries, options),
        // model_change / thinking_level_change / active_tools_change → no context
        // message (they only shape the derived state).
        _ => Vec::new(),
    }
}

/// `createCompactionSummaryMessage(entry.summary, entry.tokensBefore, entry.timestamp)`
/// plus the retained tail. Mirrors the TS `compaction` branch.
fn compaction_to_context_messages(c: &CompactionEntry) -> Vec<AgentMessage> {
    let mut out = Vec::with_capacity(c.retained_tail.len() + 1);
    out.push(create_compaction_summary_message(
        &c.summary,
        c.tokens_before,
        c.base.timestamp,
    ));
    out.extend(c.retained_tail.iter().cloned());
    out
}

/// `createBranchSummaryMessage(entry.summary, entry.fromId, entry.timestamp)` —
/// only when `entry.summary` is non-empty (mirrors TS `entry.summary && [...]`).
fn branch_summary_to_context_messages(b: &BranchSummaryEntry) -> Vec<AgentMessage> {
    if b.summary.is_empty() {
        return Vec::new();
    }
    vec![create_branch_summary_message(&b.summary, &b.from_id, b.base.timestamp)]
}

/// Custom entry → registered projector's messages, or `[]` when no projector is
/// registered for `entry.customType`. Mirrors TS `entryProjectors?.[customType] ?? []`.
fn custom_to_context_messages(
    c: &CustomEntry,
    index: usize,
    entries: &[Entry],
    options: &SessionContextBuildOptions,
) -> Vec<AgentMessage> {
    match options.entry_projectors.get(&c.custom_type) {
        Some(projector) => projector(c, index, entries),
        None => Vec::new(),
    }
}

/// `deriveSessionContextState` — walk the FULL path (not the truncated context
/// entries) to capture the latest `thinking_level`/`model`/`active_tool_names`.
/// Mirrors TS `deriveSessionContextState`.
fn derive_session_context_state(path_entries: &[Entry]) -> SessionContext {
    let mut thinking_level = "off".to_string();
    let mut model: Option<(String, String)> = None;
    let mut active_tool_names: Option<Vec<String>> = None;
    for entry in path_entries {
        match entry {
            Entry::ThinkingLevel(t) => thinking_level = t.thinking_level.clone(),
            Entry::ModelChange(m) => model = Some((m.provider.clone(), m.model_id.clone())),
            Entry::Message(m) => {
                if let AgentMessage::Assistant(a) = &m.message {
                    model = Some((a.provider.as_str().to_string(), a.model.clone()));
                }
            }
            Entry::ActiveTools(a) => active_tool_names = Some(a.active_tool_names.clone()),
            _ => {}
        }
    }
    SessionContext {
        messages: Vec::new(),
        thinking_level,
        model,
        active_tool_names,
    }
}

/// `buildSessionContext(pathEntries, options)` — derive state from the full path,
/// build context entries (default transform + caller transforms), flatMap each
/// to context messages. Mirrors TS `buildSessionContext`.
pub fn build_session_context(path_entries: &[Entry], options: &SessionContextBuildOptions) -> SessionContext {
    let mut ctx = derive_session_context_state(path_entries);
    let context_entries = build_context_entries(path_entries, options);
    let mut messages = Vec::new();
    for (index, entry) in context_entries.iter().enumerate() {
        messages.extend(session_entry_to_context_messages(entry, index, &context_entries, options));
    }
    ctx.messages = messages;
    ctx
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_ai::types::AssistantMessage;
    use pi_agent::message::AgentMessage;

    fn user(text: &str) -> AgentMessage {
        AgentMessage::User(pi_ai::types::UserMessage::new(text, 1))
    }

    fn assistant(text: &str) -> AgentMessage {
        let mut a = AssistantMessage::empty(pi_ai::types::Api::Faux, "anthropic", "claude-sonnet-4-5", 1);
        a.content = vec![pi_ai::types::Content::text(text)];
        a.stop_reason = pi_ai::types::StopReason::Stop;
        AgentMessage::Assistant(Box::new(a))
    }

    /// Build a message entry with a given id/parent (storage fields stamped
    /// minimally — only what `build_session_context` reads).
    fn msg_entry(id: &str, parent: Option<&str>, message: AgentMessage, seq: u64) -> Entry {
        use crate::session::types::{EntryBase, MessageEntry};
        Entry::Message(MessageEntry {
            base: EntryBase {
                entry_type: "message".into(),
                id: id.into(),
                seq,
                parent_id: parent.map(|s| s.into()),
                timestamp: seq as i64,
            },
            message,
            terminate: None,
        })
    }

    fn msg_entry_p(id: &str, parent: &str, message: AgentMessage, seq: u64) -> Entry {
        msg_entry(id, Some(parent), message, seq)
    }

    fn compaction_entry(id: &str, parent: &str, summary: &str, tail: Vec<AgentMessage>, seq: u64) -> Entry {
        use crate::session::types::{EntryBase, CompactionEntry};
        Entry::Compaction(CompactionEntry {
            base: EntryBase {
                entry_type: "compaction".into(),
                id: id.into(),
                seq,
                parent_id: Some(parent.into()),
                timestamp: seq as i64,
            },
            summary: summary.into(),
            retained_tail: tail,
            tokens_before: 100,
            details: None,
            usage: None,
        })
    }

    fn model_change(id: &str, parent: &str, provider: &str, model_id: &str, seq: u64) -> Entry {
        use crate::session::types::{EntryBase, ModelChangeEntry};
        Entry::ModelChange(ModelChangeEntry {
            base: EntryBase {
                entry_type: "model_change".into(),
                id: id.into(),
                seq,
                parent_id: Some(parent.into()),
                timestamp: seq as i64,
            },
            provider: provider.into(),
            model_id: model_id.into(),
        })
    }

    fn thinking_level(id: &str, parent: &str, level: &str, seq: u64) -> Entry {
        use crate::session::types::{EntryBase, ThinkingLevelEntry};
        Entry::ThinkingLevel(ThinkingLevelEntry {
            base: EntryBase {
                entry_type: "thinking_level_change".into(),
                id: id.into(),
                seq,
                parent_id: Some(parent.into()),
                timestamp: seq as i64,
            },
            thinking_level: level.into(),
        })
    }

    fn branch_summary_entry(id: &str, parent: &str, from_id: &str, summary: &str, seq: u64) -> Entry {
        use crate::session::types::{EntryBase, BranchSummaryEntry};
        Entry::BranchSummary(BranchSummaryEntry {
            base: EntryBase {
                entry_type: "branch_summary".into(),
                id: id.into(),
                seq,
                parent_id: Some(parent.into()),
                timestamp: seq as i64,
            },
            from_id: from_id.into(),
            summary: summary.into(),
            details: None,
            usage: None,
        })
    }

    fn custom_entry(id: &str, parent: &str, custom_type: &str, data: &str, seq: u64) -> Entry {
        use crate::session::types::{EntryBase, CustomEntry};
        Entry::Custom(CustomEntry {
            base: EntryBase {
                entry_type: "custom".into(),
                id: id.into(),
                seq,
                parent_id: Some(parent.into()),
                timestamp: seq as i64,
            },
            custom_type: custom_type.into(),
            data: Some(serde_json::Value::String(data.into())),
        })
    }

    #[test]
    fn starts_at_latest_compaction_and_materializes_retained_tail() {
        let entries = vec![
            msg_entry("old", None, user("old"), 1),
            compaction_entry("compact", "old", "summary", vec![user("retained"), assistant("answer")], 2),
            model_change("model", "compact", "openai", "gpt-5", 3),
            thinking_level("thinking", "model", "high", 4),
            msg_entry_p("tail", "thinking", user("tail"), 5),
        ];
        let ctx = build_session_context(&entries, &SessionContextBuildOptions::default());
        assert_eq!(
            ctx.messages.iter().map(|m| m.role().as_str().to_string()).collect::<Vec<_>>(),
            ["compactionSummary", "user", "assistant", "user"]
        );
        assert_eq!(ctx.model, Some(("openai".to_string(), "gpt-5".to_string())));
        assert_eq!(ctx.thinking_level, "high");
    }

    #[test]
    fn applies_caller_transforms_after_compaction_boundary() {
        let entries = vec![
            msg_entry("old", None, user("old"), 1),
            compaction_entry("compact", "old", "summary", vec![], 2),
            branch_summary_entry("branch", "compact", "abandoned", "branch summary", 3),
            msg_entry_p("tail", "branch", user("tail"), 4),
        ];
        let drop_compaction: ContextEntryTransform =
            Arc::new(|ctx_entries: &[Entry]| ctx_entries.iter().filter(|e| !matches!(e, Entry::Compaction(_))).cloned().collect());
        let opts = SessionContextBuildOptions {
            entry_transforms: vec![drop_compaction],
            entry_projectors: BTreeMap::new(),
        };
        let ctx = build_session_context(&entries, &opts);
        assert_eq!(
            ctx.messages.iter().map(|m| m.role().as_str().to_string()).collect::<Vec<_>>(),
            ["branchSummary", "user"]
        );
    }

    #[test]
    fn projects_custom_entries_and_omits_deferred_assistant() {
        // Deferred assistant handle → dropped (not model-visible).
        let mut deferred = AssistantMessage::empty(pi_ai::types::Api::Faux, "openai", "gpt-5", 1);
        deferred.stop_reason = pi_ai::types::StopReason::Deferred;
        deferred.deferred = Some(pi_ai::types::DeferredHandle {
            provider: "openai".into(),
            model_id: "gpt-5".into(),
            api: "openai-responses".into(),
            id: "response-1".into(),
            expires_at: None,
            poll_after_ms: None,
            data: None,
        });
        let entries = vec![
            msg_entry("user", None, user("hello"), 1),
            msg_entry_p("deferred", "user", AgentMessage::Assistant(Box::new(deferred)), 2),
            custom_entry("custom", "deferred", "note", "project me", 3),
        ];
        let projector: CustomEntryContextMessageProjector = Arc::new(|c: &CustomEntry, _, _| {
            vec![user(&format!("note: {}", c.data.clone().unwrap_or(serde_json::Value::Null).to_string().trim_matches('"')))]
        });
        let mut projectors = BTreeMap::new();
        projectors.insert("note".to_string(), projector);
        let opts = SessionContextBuildOptions { entry_transforms: vec![], entry_projectors: projectors };
        let ctx = build_session_context(&entries, &opts);
        assert_eq!(
            ctx.messages.iter().map(|m| m.role().as_str().to_string()).collect::<Vec<_>>(),
            ["user", "user"]
        );
    }
}
