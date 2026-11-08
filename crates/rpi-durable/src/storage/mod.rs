//! Host-owned durable journal storage.
//!
//! The harness owns the Pi-facing `Session` abstraction and converts transcript
//! or execution mutations into [`JournalFrame`] values. This module owns the
//! persistence contract and concrete JSONL/SQLite stores. It deliberately knows
//! nothing about `AgentMessage` or the harness session tree.

mod backend;
pub mod jsonl;
#[cfg(feature = "sqlite")]
pub mod sqlite;

pub use backend::{BackendError, DurableBackend, JournalFrame, JournalMutation};
