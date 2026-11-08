//! Read-side reconciliation boundary.
//!
//! Reconciliation here means validating a complete durable control view before
//! exposing it to a host. It does not add an external effect protocol or a
//! second recovery model.

pub use super::receipt::{ControlRecord, VerifiedJournal};
