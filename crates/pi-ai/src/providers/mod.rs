//! Concrete providers. Mirrors `packages/ai/src/api/*` + the in-tree faux
//! provider (`packages/agent/src/faux.ts`, hoisted here so every crate's tests
//! can reuse it without depending on `pi-agent`).
//!
//! Faux is always available. HTTP providers are enabled by the `providers`
//! feature.

pub mod faux;

#[cfg(feature = "providers")]
pub mod anthropic;

#[cfg(feature = "providers")]
pub mod openai_completions;

#[cfg(feature = "providers")]
pub mod openai_responses;
