//! `session` submodule — mirrors `packages/agent/src/harness/session/*`.
//!
//! Durable session model ([`types`]), the in-memory state reducer ([`state`])
//! + record-log validator ([`reducer`]), the in-memory backend ([`memory`]),
//! the JSONL codec + storage + repo ([`jsonl`]), and the `Session` + `SessionTree`
//! facade ([`session`]) built over a [`types::SessionStorage`].
//!
//! Built up incrementally across M5b–M5f.

pub mod memory;
pub mod reducer;
pub mod state;
pub mod types;

pub use memory::{
    Clock, CounterIdGenerator, FakeClock, InMemorySessionRepo, InMemorySessionStorage, SystemClock,
};
pub use reducer::{RecordLogCorruption, RecordLogCorruptionReason, RecordLogSlice, validate_record_log};
pub use state::{ApplyOutcome, SessionState};
