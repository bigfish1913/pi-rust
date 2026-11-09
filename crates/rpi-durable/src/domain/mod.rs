//! Backend-independent durable domain state.
//!
//! These modules contain validation, state transitions and proof primitives;
//! they do not open files, know about AgentMessage, or own a session backend.

pub mod control;
pub mod execution;
pub mod ledger;
pub mod progress;
