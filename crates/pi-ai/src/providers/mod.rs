//! Concrete providers. Mirrors `packages/ai/src/api/*` + the in-tree faux
//! provider (`packages/agent/src/faux.ts`, hoisted here so every crate's tests
//! can reuse it without depending on `pi-agent`).
//!
//! v1 ships faux (always-on) + anthropic (behind the `providers` feature).
//! Other providers (openai/google/bedrock) leave the trait seam in
//! [`crate::provider`] for the future.

pub mod faux;

#[cfg(feature = "providers")]
pub mod anthropic;
