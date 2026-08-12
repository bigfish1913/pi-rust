//! Mirrors `packages/ai/src` — the unified, multi-provider LLM layer.
//!
//! Provider-agnostic types (`Content`, `Message`, `AssistantMessage`,
//! `AssistantMessageEvent`, `Context`, `Tool`, `Usage`) live in [`types`];
//! model-layer configuration (`Model`, compat structs) lives in [`model`].
//! Schema validation + LLM-sloppy-JSON coercion (mirroring `validation.ts`) is
//! in [`schema`]; the SPSC streaming queue mirroring `event-stream.ts` is in
//! [`event_stream`]; the `Provider` trait seam is in [`provider`]; concrete
//! providers live under [`providers`] (faux now, anthropic in M3).
//!
//! Every module's doc comment names the TS file it mirrors.

pub mod error;
pub mod event_stream;
pub mod model;
pub mod provider;
pub mod providers;
pub mod schema;
pub mod types;

// Flat re-exports — consumers do `use pi_ai::Message` not `pi_ai::types::Message`.
pub use error::AiError;
pub use event_stream::{AssistantMessageEventStream, AssistantMessageEventStreamProducer};
pub use model::{Model, StreamingProtocolCompat};
pub use provider::{Provider, SimpleStreamOptions, CacheRetention};
pub use schema::validate_tool_arguments;
pub use types::{
    Api, AssistantMessage, AssistantMessageEvent, Content, Context, DeferredHandle, DoneReason,
    ErrorReason, ImageContent, InputModality, Message, ModelCost, ModelCostRates,
    ModelCostTier, ProviderId, Schema, StopReason, TextContent, ThinkingBudgets,
    ThinkingContent, ThinkingLevel, ThinkingLevelMap, Tool, ToolCall, ToolCallType,
    ToolResultMessage, Usage, UsageCost, UserContent, UserMessage,
};
