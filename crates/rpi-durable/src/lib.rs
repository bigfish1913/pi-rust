//! Native durable authority primitives and storage contracts.
//!
//! The host owns append acknowledgement, branch identity and terminal decisions.
//! `domain` remains backend-independent; `runtime` and `storage` provide the
//! host-owned commit boundary without depending on Agent Loop types. The
//! harness supplies the Pi-facing Session facade and adapts its records to this
//! crate's journal boundary.

pub mod domain;
pub mod runtime;
pub mod storage;

// Compatibility exports for the pre-domain module layout. New code should use
// `rpi_durable::domain::*`; keeping these paths avoids a flag-day migration.
pub use domain::{control, execution, ledger, progress};
