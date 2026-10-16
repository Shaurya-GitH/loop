//! Stateful agent with tool execution and event streaming.
//!
//! Built on [`loop_ai`]. See the crate README for the public mental model
//! (agent loop, stateful [`Agent`], and [`harness::AgentHarness`]).

#![deny(missing_docs)]

pub mod agent;
pub mod agent_loop;
pub mod harness;
pub mod messages;
pub mod native_backend;
pub mod protocol_events;
pub mod stream_fn;
#[cfg(feature = "telemetry")]
mod telemetry;
#[cfg(not(feature = "telemetry"))]
#[path = "telemetry_noop.rs"]
mod telemetry;
pub mod types;

#[cfg(feature = "proxy")]
pub mod proxy;

pub use agent::{Agent, AgentError, AgentOptions};
pub use agent_loop::{
    agent_loop, agent_loop_continue, collect_agent_events, run_agent_loop, run_agent_loop_continue,
    AgentEventStream, AgentLoopError,
};
pub use messages::{
    bash_execution_to_text, convert_to_llm, create_branch_summary_message,
    create_compaction_summary_message, create_custom_message, user_message_with_images,
    BRANCH_SUMMARY_PREFIX, BRANCH_SUMMARY_SUFFIX, COMPACTION_SUMMARY_PREFIX,
    COMPACTION_SUMMARY_SUFFIX,
};
pub use stream_fn::{
    clear_default_stream_fn, get_default_stream_fn, set_default_stream_fn, stream_fn_from_models,
    StreamFn,
};
pub use types::*;

/// Re-export commonly used loop-ai types for agent consumers.
pub use loop_ai::{
    new_id, now_ms, AssistantMessage, AssistantMessageEvent, Context, Message, Model, Models,
    SimpleStreamOptions, StopReason, ToolCall, ToolResultMessage,
};
