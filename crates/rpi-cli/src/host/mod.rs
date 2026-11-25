//! Shared-host building blocks, compiled into rpi rather than a separate crate.
//!
//! This layer defines lifecycle rules, capacity, a persistent administrative
//! catalog, supervised ordinary Harness backend, native admission/receipts,
//! immutable trusted profiles, a bounded trusted administrative facade and a
//! control daemon/CLI. Importing this module starts no services. The journal alone
//! dispatches nothing; explicit daemon policy owns native administration. IPC
//! exposes allowed profile refs, full-intent receipts and journalled profile
//! create/restore/guarded close, cancel and stop. Session-free Mini has separate
//! opt-in private authority and v1.6 volatile operations; never durable replay.

#[cfg(unix)]
pub mod administration;
#[cfg(target_os = "macos")]
mod area;
pub(crate) mod area_entry;
#[cfg(unix)]
mod area_rpc;
#[cfg(target_os = "macos")]
mod area_terminal;
pub mod bindings;
mod capacity;
mod catalog;
pub(crate) mod cli;
pub(crate) mod cli_cleanup;
pub(crate) mod cli_resources;
pub(crate) mod cli_tasks;
/// Versioned shared Host control client; not native session RPC.
pub mod client;
mod config;
#[cfg(unix)]
pub mod daemon;
#[cfg(unix)]
mod daemon_administration;
pub(crate) mod dev_staging;
mod error;
#[cfg(unix)]
mod extension_artifacts;
/// Staged Darwin process-group custody; not a complete CLI cleanup receipt.
#[cfg(target_os = "macos")]
#[allow(dead_code)]
mod tool_processes;
#[cfg(unix)]
pub use extension_artifacts::{
    NativeExtensionArtifacts, NativeStagedExtensions, MAX_NATIVE_EXTENSION_ARTIFACTS,
    MAX_NATIVE_EXTENSION_BYTES, MAX_NATIVE_EXTENSION_TOTAL_BYTES,
};
mod extension_custody;
pub use extension_custody::{NativeExtensionCall, NativeExtensionCustody};
mod binding_resources;
pub use binding_resources::{NativeBindingResources, NativeHookConfig};
mod instruction_resources;
pub use instruction_resources::NativeInstructionResources;
// Private staged adapter; default CLI integration follows lifecycle tests.
#[allow(dead_code)]
mod in_process;
#[cfg(unix)]
mod launch_policy;
pub mod lifecycle;
/// Session-free owned execution; volatile results, not the persistent wire API.
pub mod mini;
#[cfg(unix)]
pub mod mini_control;
pub mod ordinary;
mod profile;
mod profile_set;
pub mod protocol;
pub use profile::{AgentProfileFile, HostStartupAgent, MAX_PROFILE_BYTES};
pub use profile_set::{PreparedProfile, TrustedProfileSet, MAX_TRUSTED_PROFILES};
mod recovery;
mod registry;
mod runtime;
#[cfg(unix)]
pub mod server;
pub mod service;
#[cfg(target_os = "macos")]
mod supervisor;
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
    AgentId, AgentMode, AgentSpec, AgentState, ControlScopeId, ExecutionMode, HostEpoch,
    LaunchSpec, ModelRef, ProfileRef, ProfileVersion, RequestId, RunId, SessionRef,
};

#[cfg(all(test, unix))]
mod registry_tests;
#[cfg(test)]
mod tests;
