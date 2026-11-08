//! `session` submodule — mirrors `packages/agent/src/harness/session/*`.
//!
//! Pi-facing Session abstraction ([`types`]), the in-memory state reducer
//! ([`state`]) + record-log validator ([`reducer`]), memory/JSONL storage,
//! the JSONL codec + repository ([`jsonl`]), the context builder ([`context`])
//! that projects a branch path to provider-facing messages, and the `Session`
//! + `SessionTree` facade ([`session`]).
//!
//! New durable persistence code should cross [`durable_adapter`] and call the
//! `rpi-durable` runtime/storage boundary. The in-crate Memory/JSONL
//! implementations remain the default Session storage paths.
//!
//! Built up incrementally across M5b–M5f.

#[cfg(test)]
mod storage_contract_tests;

pub(crate) mod branch_queries;
#[cfg(test)]
mod branch_queries_compat_tests;
#[cfg(test)]
mod branch_queries_tests;
pub mod context;
pub(crate) mod diagnostics;
pub mod durable_adapter;
pub mod jsonl;
pub mod memory;
pub(crate) mod message_preview;
pub(crate) mod progress;
pub mod reducer;
pub mod session;
pub mod state;
pub mod types;
pub mod values;

pub use context::{
    build_context_entries, build_session_context, default_context_entry_transform,
    session_entry_to_context_messages, ContextEntryTransform, CustomEntryContextMessageProjector,
    SessionContext, SessionContextBuildOptions,
};
pub use durable_adapter::{
    entries_from_frames, entry_from_frame, entry_mutation, lane_record_from_frame,
    lane_record_mutation, lane_records_from_frames, session_mutation_from_frame,
    session_mutation_to_journal_mutation, session_mutations_from_frames, DurableSessionMutationLog,
    DurableSessionWriter, SessionDurableWriter, SharedDurableSessionWriter,
};
pub use memory::{
    Clock, CounterIdGenerator, FakeClock, InMemorySessionRepo, InMemorySessionStorage, SystemClock,
};
pub use reducer::{
    validate_record_log, RecordLogCorruption, RecordLogCorruptionReason, RecordLogSlice,
};
pub use session::{DefaultIdGenerator, Session};
pub use state::{ApplyOutcome, SessionState};
pub use values::{SessionValueWriter, SessionValues, VALUE_CUSTOM_TYPE};
pub mod keyed_queue;
pub mod resources;
pub mod search;
