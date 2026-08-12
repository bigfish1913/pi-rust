//! Mirrors `packages/agent/src/harness` (AgentHarness: session tree, JSONL
//! persistence, compaction, the run loop that drives `pi-agent`).
//!
//! M5 surface, built up module-by-module. Currently exposes the foundation
//! layers (`error`, `result`, `types`, `session::types`); subsequent modules
//! (`session::state`, `session::reducer`, `session::jsonl`, `compaction`,
//! `messages`, `events`, `skills`, `agent_harness`) are added as M5 progresses.

pub mod error;
pub mod result;
pub mod session;
pub mod types;
