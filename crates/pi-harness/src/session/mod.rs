//! `session` submodule — mirrors `packages/agent/src/harness/session/*`.
//!
//! Durable session model ([`types`]), the in-memory state reducer ([`state`])
//! + record-log validator ([`reducer`]), the in-memory backend ([`memory`]),
//! the JSONL codec + storage + repo ([`jsonl`]), the context builder
//! ([`context`]) that projects a branch path to provider-facing messages, and
//! the `Session` + `SessionTree` facade ([`session`]) built over a
//! [`types::SessionStorage`].
//!
//! Built up incrementally across M5b–M5f.

pub mod context;
pub mod jsonl;
pub mod memory;
pub mod reducer;
pub mod session;
pub mod state;
pub mod types;

pub use context::{
    build_context_entries, build_session_context, default_context_entry_transform,
    session_entry_to_context_messages, ContextEntryTransform, CustomEntryContextMessageProjector,
    SessionContext, SessionContextBuildOptions,
};
pub use memory::{
    Clock, CounterIdGenerator, FakeClock, InMemorySessionRepo, InMemorySessionStorage, SystemClock,
};
pub use reducer::{RecordLogCorruption, RecordLogCorruptionReason, RecordLogSlice, validate_record_log};
pub use session::{DefaultIdGenerator, Session};
pub use state::{ApplyOutcome, SessionState};
