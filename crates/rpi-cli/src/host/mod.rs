//! Shared-host building blocks, compiled into rpi rather than a separate crate.
//!
//! This layer defines lifecycle rules, capacity, a persistent administrative
//! catalog and a supervised ordinary Harness backend. IPC, a service coordinator,
//! mini execution and CLI flags are not enabled by importing this module.

mod capacity;
mod catalog;
mod config;
mod error;
pub mod ordinary;
mod registry;
mod types;

pub use capacity::{Capacity, CapacitySnapshot, ResidentPermit};
pub use catalog::{
    AgentPage, AgentRecord, CatalogGuard, CatalogStatus, HostCatalog, MAX_PAGE_SIZE,
};
pub use config::{
    EffectiveTimeout, EffectiveTimeouts, HostConfig, TimeoutOverride, TimeoutOverrides,
    TimeoutSource, Timeouts,
};
pub use error::{CatalogError, CatalogResult, HostError, HostResult};
pub use types::{
    AgentId, AgentMode, AgentSpec, AgentState, ExecutionMode, HostEpoch, LaunchSpec, ModelRef,
    RequestId, RunId, SessionRef,
};

#[cfg(all(test, unix))]
mod registry_tests;
#[cfg(test)]
mod tests;
