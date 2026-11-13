//! Backend-neutral runtime boundary for durable writes.
//!
//! The Pi-facing Session and actor-facing host APIs remain in `rpi-harness`
//! during the staged migration. This module owns the small runtime adapter
//! that turns storage CAS into a single-writer append flow.

pub mod actor;
pub mod admission;
pub mod receipt;
pub mod reconcile;
pub mod scope;
pub mod service;

pub const CONTROL_FORMAT: &str = "rpi-native-durable-control";
pub const MAX_RECEIPTS: usize = 64;

pub use actor::DurableActor;
pub use admission::{
    validate_execution_target, validate_identity, validate_run_id, DurableError, DurableMutation,
};
pub use receipt::{DurableOutcome, DurableReceipt};
pub use reconcile::{ControlRecord, VerifiedJournal};
pub use scope::{
    DurableExecutionCursor, DurableExecutionIndexSnapshot, DurableExecutionPage,
    DurableExecutionSnapshot, DurableRunSummary, DurableScope, DurableSnapshot,
};
pub use service::DurableService;
