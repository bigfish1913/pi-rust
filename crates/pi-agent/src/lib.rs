//! Mirrors `packages/agent/src` (core layer: agent + agent-loop + types + stream-fn).
//!
//! Provider-agnostic agent runtime. The only LLM boundary is [`StreamFn`]
//! (sync return → [`AssistantMessageEventStream`]); everything else talks in
//! [`AgentMessage`] and never touches a provider wire format.
//!
//! M2 scope per the plan: `AgentTool` trait, `Agent` + `AgentBuilder`,
//! `run_agent_loop` / `run_agent_loop_continue` free functions, events, hooks,
//! queues, abort. The critical invariants — tool-execution ordering
//! (end in completion order, tool-result `MessageEnd` in source/ordinal order),
//! truncate-fail, and late-update suppression — live in [`agent_loop`].

pub mod abort;
pub mod agent;
pub mod agent_loop;
pub mod agent_tool;
pub mod error;
pub mod events;
pub mod hooks;
pub mod message;
pub mod queue;
pub mod stream_fn;
pub mod types;

pub use abort::AbortHandle;
pub use agent::{Agent, AgentBuilder, AgentOptions};
pub use agent_loop::{run_agent_loop, run_agent_loop_continue, LoopOutcome, NewMessages};
pub use agent_tool::AgentTool;
pub use error::AgentError;
pub use events::{AgentEmitter, AgentEvent, CollectorEmitter};
pub use hooks::{
    AfterToolCall, AfterToolResults, AgentLoopConfig, BeforeToolCall, ConvertToLlm, GetApiKey,
    GetFollowUpMessages, GetSteeringMessages, PrepareNextTurn, ShouldStopAfterTurn,
    TransformContext,
};
pub use message::{AgentMessage, AgentMessageRole, CustomMessage};
pub use queue::PendingMessageQueue;
pub use stream_fn::{stream_fn, StreamFn};
pub use types::{
    AfterToolCallContext, AfterToolCallResult, AgentContext, AgentLoopTurnUpdate, AgentState,
    AgentToolResult, BeforeToolCallContext, BeforeToolCallResult, QueueMode,
    ShouldStopAfterTurnContext, TextContentOrImage, ToolExecutionMode, ToolResultPartial,
};
