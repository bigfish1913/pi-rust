//! Mirrors `packages/agent/src/harness` (AgentHarness: session tree, JSONL
//! persistence, compaction, the run loop that drives `pi-agent`).
//!
//! M5 surface, built up module-by-module. Currently exposes the foundation
//! layers (`error`, `result`, `types`, `session::types`); subsequent modules
//! (`session::state`, `session::reducer`, `session::jsonl`, `compaction`,
//! `messages`, `events`, `skills`, `agent_harness`) are added as M5 progresses.

pub mod agent_harness;
pub mod compaction;
pub mod context_files;
pub mod error;
pub mod events;
pub mod frontmatter;
pub mod messages;
pub mod prompt_templates;
pub mod result;
pub mod session;
pub mod skills;
pub mod system_prompt;
pub mod types;
