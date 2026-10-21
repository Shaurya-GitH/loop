//! Core agent types: events, tools, context, and loop configuration.

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use loop_ai::{
    AssistantMessage, AssistantMessageEvent, ImageContent, Message, Model, SimpleStreamOptions,
    TextContent, ThinkingBudgets, ThinkingLevel, Tool, ToolCall, ToolResultContent,
    ToolResultMessage, Usage,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

/// How tool calls from a single assistant message are executed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum ToolExecutionMode {
    /// Prepare, execute, and finalize each tool call before the next starts.
    Sequential,
    /// Prepare sequentially, execute concurrently; end events by completion order.
    #[default]
    Parallel,
}

/// How many queued messages to inject at a drain point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum QueueMode {
    /// Drain every queued message.
    All,
    /// Drain only the oldest queued message.
    #[default]
    #[serde(rename = "one-at-a-time")]
    OneAtATime,
}

/// Agent thinking level including an explicit off state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum AgentThinkingLevel {
    /// Thinking disabled.
    #[default]
    Off,
    /// Minimal reasoning.
    Minimal,
    /// Low reasoning.
    Low,
    /// Medium reasoning.
    Medium,
    /// High reasoning.
    High,
    /// Extra-high reasoning.
    #[serde(rename = "xhigh")]
    XHigh,
    /// Maximum reasoning.
    Max,
}

impl AgentThinkingLevel {
    /// Map to loop-ai reasoning level (`None` when off).
    pub fn to_reasoning(self) -> Option<ThinkingLevel> {
        match self {
            Self::Off => None,
            Self::Minimal => Some(ThinkingLevel::Minimal),
            Self::Low => Some(ThinkingLevel::Low),
            Self::Medium => Some(ThinkingLevel::Medium),
            Self::High => Some(ThinkingLevel::High),
            Self::XHigh => Some(ThinkingLevel::XHigh),
            Self::Max => Some(ThinkingLevel::Max),
        }
    }
}

/// Result returned from `before_tool_call`.
#[derive(Debug, Clone, Default)]
pub struct BeforeToolCallResult {
    /// When true, prevent execution.
    pub block: bool,
    /// Reason shown in the error tool result.
    pub reason: Option<String>,
}

/// Partial override returned from `after_tool_call` (field-by-field replace).
#[derive(Debug, Clone, Default)]
pub struct AfterToolCallResult {
    /// Replace content array.
    pub content: Option<Vec<ToolResultContent>>,
    /// Replace details.
    pub details: Option<Value>,
    /// Replace error flag.
    pub is_error: Option<bool>,
    /// Replace usage.
    pub usage: Option<Usage>,
    /// Replace terminate hint.
    pub terminate: Option<bool>,
}

/// Context passed to `before_tool_call`.
#[derive(Clone)]
pub struct BeforeToolCallContext {
    /// Assistant message that requested the call.
    pub assistant_message: AssistantMessage,
    /// Raw tool call block.
    pub tool_call: ToolCall,
    /// Validated arguments.
    pub args: Value,
    /// Current agent context.
    pub context: AgentContext,
}

impl BeforeToolCallContext {
    /// Context for a call that did not come from a model turn (an MCP
    /// client, a workflow shell task): a placeholder assistant message that
    /// holds just this call.
    pub fn standalone(tool_call: ToolCall, args: Value) -> Self {
        Self {
            assistant_message: AssistantMessage {
                content: vec![loop_ai::AssistantContent::ToolCall(tool_call.clone())],
                api: String::new(),
                provider: String::new(),
                model: String::new(),
                response_model: None,
                response_id: None,
                usage: Default::default(),
                stop_reason: loop_ai::StopReason::ToolUse,
                error_message: None,
                raw_stop_reason: None,
                timestamp: chrono::Utc::now().timestamp_millis(),
            },
            tool_call,
            args,
            context: AgentContext::default(),
        }
    }
}

/// Context passed to `after_tool_call`.
#[derive(Clone)]
pub struct AfterToolCallContext {
    /// Assistant message that requested the call.
    pub assistant_message: AssistantMessage,
    /// Raw tool call block.
    pub tool_call: ToolCall,
    /// Validated arguments.
    pub args: Value,
    /// Executed result before overrides.
    pub result: AgentToolResult,
    /// Whether currently treated as error.
    pub is_error: bool,
    /// Current agent context.
    pub context: AgentContext,
}

/// Context passed to `should_stop_after_turn` / `prepare_next_turn`.
#[derive(Clone)]
pub struct ShouldStopAfterTurnContext {
    /// Assistant message that completed the turn.
    pub message: AssistantMessage,
    /// Tool results for the turn.
    pub tool_results: Vec<ToolResultMessage>,
    /// Current context after appends.
    pub context: AgentContext,
    /// Messages produced by this loop invocation.
    pub new_messages: Vec<AgentMessage>,
}

/// Replacement runtime state for the next turn.
#[derive(Clone, Default)]
pub struct AgentLoopTurnUpdate {
    /// Context for the next provider request.
    pub context: Option<AgentContext>,
    /// Model for the next request.
    pub model: Option<Model>,
    /// Thinking level for the next request.
    pub thinking_level: Option<AgentThinkingLevel>,
}

/// Final or partial result produced by a tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentToolResult {
    /// Content returned to the model.
    pub content: Vec<ToolResultContent>,
    /// Structured details for logs/UI.
    pub details: Value,
    /// Optional usage from the tool itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// Tool names introduced by this result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added_tool_names: Option<Vec<String>>,
    /// Hint to stop after the current batch (all-agree).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminate: Option<bool>,
}

impl AgentToolResult {
    /// Create a text success result.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![ToolResultContent::Text(TextContent {
                text: text.into(),
                text_signature: None,
            })],
            details: Value::Object(Default::default()),
            usage: None,
            added_tool_names: None,
            terminate: None,
        }
    }

    /// Create an error-shaped result (content only; caller sets is_error).
    pub fn error_text(message: impl Into<String>) -> Self {
        Self::text(message)
    }
}

/// Callback for streaming tool progress updates.
pub type AgentToolUpdateCallback = Arc<dyn Fn(AgentToolResult) + Send + Sync>;

/// Async tool execute function.
pub type AgentToolExecuteFn = Arc<
    dyn Fn(
            String,
            Value,
            Option<CancellationToken>,
            Option<AgentToolUpdateCallback>,
        ) -> Pin<Box<dyn Future<Output = Result<AgentToolResult, String>> + Send>>
        + Send
        + Sync,
>;

/// Tool definition used by the agent runtime.
#[derive(Clone)]
pub struct AgentTool {
    /// Tool name (must match model tool call).
    pub name: String,
    /// Human-readable label for UI.
    pub label: String,
    /// Description for the model.
    pub description: String,
    /// JSON Schema parameters.
    pub parameters: Value,
    /// Optional per-tool execution mode override.
    pub execution_mode: Option<ToolExecutionMode>,
    /// Optional arg shim before schema validation.
    pub prepare_arguments: Option<Arc<dyn Fn(Value) -> Value + Send + Sync>>,
    /// Execute the tool. Return `Err` on failure (encoded as is_error tool result).
    pub execute: AgentToolExecuteFn,
}

impl AgentTool {
    /// Convert to a loop-ai [`Tool`] for the LLM context.
    pub fn to_llm_tool(&self) -> Tool {
        Tool {
            name: self.name.clone(),
            description: self.description.clone(),
            parameters: self.parameters.clone(),
        }
    }

    /// Build a simple tool from an async closure.
    pub fn simple<F, Fut>(
        name: impl Into<String>,
        label: impl Into<String>,
        description: impl Into<String>,
        parameters: Value,
        execute: F,
    ) -> Self
    where
        F: Fn(String, Value, Option<CancellationToken>, Option<AgentToolUpdateCallback>) -> Fut
            + Send
            + Sync
            + 'static,
        Fut: Future<Output = Result<AgentToolResult, String>> + Send + 'static,
    {
        let execute = Arc::new(move |id, args, cancel, on_update| {
            let fut = execute(id, args, cancel, on_update);
            Box::pin(fut) as Pin<Box<dyn Future<Output = Result<AgentToolResult, String>> + Send>>
        });
        Self {
            name: name.into(),
            label: label.into(),
            description: description.into(),
            parameters,
            execution_mode: None,
            prepare_arguments: None,
            execute,
        }
    }
}

/// Custom agent message roles beyond LLM messages.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "camelCase")]
pub enum CustomAgentMessage {
    /// Bash execution display message.
    #[serde(rename = "bashExecution")]
    BashExecution {
        /// Command that was run.
        command: String,
        /// Captured output text.
        output: String,
        /// Exit code if known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
        /// Unix epoch ms.
        timestamp: i64,
    },
    /// App-specific custom message.
    Custom {
        /// Custom type name.
        #[serde(rename = "customType")]
        custom_type: String,
        /// Content text.
        content: String,
        /// Unix epoch ms.
        timestamp: i64,
        /// Optional details.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<Value>,
    },
    /// Branch summary injected into context.
    #[serde(rename = "branchSummary")]
    BranchSummary {
        /// Summary text.
        summary: String,
        /// Unix epoch ms.
        timestamp: i64,
    },
    /// Compaction summary injected into context.
    #[serde(rename = "compactionSummary")]
    CompactionSummary {
        /// Summary text.
        summary: String,
        /// Unix epoch ms.
        timestamp: i64,
    },
}

impl CustomAgentMessage {
    /// Role string.
    pub fn role(&self) -> &'static str {
        match self {
            Self::BashExecution { .. } => "bashExecution",
            Self::Custom { .. } => "custom",
            Self::BranchSummary { .. } => "branchSummary",
            Self::CompactionSummary { .. } => "compactionSummary",
        }
    }
}

/// Agent transcript message: LLM message or custom role.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
#[allow(
    clippy::large_enum_variant,
    reason = "public serde type; boxing variants would break the API"
)]
pub enum AgentMessage {
    /// Standard LLM message.
    Llm(Message),
    /// Custom / harness message.
    Custom(CustomAgentMessage),
}

impl AgentMessage {
    /// Role string for debugging / continue validation.
    pub fn role(&self) -> &'static str {
        match self {
            Self::Llm(m) => m.role(),
            Self::Custom(m) => m.role(),
        }
    }

    /// Convenience user text.
    pub fn user_text(text: impl Into<String>) -> Self {
        Self::Llm(Message::user_text(text))
    }

    /// Wrap an assistant message.
    pub fn assistant(msg: AssistantMessage) -> Self {
        Self::Llm(Message::Assistant(msg))
    }

    /// Wrap a tool result.
    pub fn tool_result(msg: ToolResultMessage) -> Self {
        Self::Llm(Message::ToolResult(msg))
    }

    /// Try as LLM message.
    pub fn as_llm(&self) -> Option<&Message> {
        match self {
            Self::Llm(m) => Some(m),
            Self::Custom(_) => None,
        }
    }

    /// Try as assistant message.
    pub fn as_assistant(&self) -> Option<&AssistantMessage> {
        match self {
            Self::Llm(Message::Assistant(a)) => Some(a),
            _ => None,
        }
    }
}

impl From<Message> for AgentMessage {
    fn from(value: Message) -> Self {
        Self::Llm(value)
    }
}

impl From<AssistantMessage> for AgentMessage {
    fn from(value: AssistantMessage) -> Self {
        Self::assistant(value)
    }
}

impl From<ToolResultMessage> for AgentMessage {
    fn from(value: ToolResultMessage) -> Self {
        Self::tool_result(value)
    }
}

/// Context snapshot passed into the low-level agent loop.
#[derive(Clone, Default)]
pub struct AgentContext {
    /// System prompt.
    pub system_prompt: String,
    /// Transcript.
    pub messages: Vec<AgentMessage>,
    /// Tools available for this run.
    pub tools: Option<Vec<AgentTool>>,
}

/// Events emitted by the agent for UI updates.
#[derive(Debug, Clone)]
#[allow(
    clippy::large_enum_variant,
    reason = "public serde type; boxing variants would break the API"
)]
pub enum AgentEvent {
    /// Agent begins processing.
    AgentStart,
    /// Final event for the run.
    AgentEnd {
        /// Messages produced by this run.
        messages: Vec<AgentMessage>,
    },
    /// Lightweight status for UI (prep phases, waiting on model, etc.).
    Progress {
        /// One-line human-readable step description.
        message: String,
    },
    /// New turn begins.
    TurnStart,
    /// Turn completes.
    TurnEnd {
        /// Assistant message for the turn.
        message: AgentMessage,
        /// Tool results from the turn.
        tool_results: Vec<ToolResultMessage>,
    },
    /// Any message begins.
    MessageStart {
        /// Message.
        message: AgentMessage,
    },
    /// Assistant streaming update.
    MessageUpdate {
        /// Partial/current message.
        message: AgentMessage,
        /// Nested AI stream event.
        assistant_message_event: AssistantMessageEvent,
    },
    /// Message completes.
    MessageEnd {
        /// Final message.
        message: AgentMessage,
    },
    /// Tool begins.
    ToolExecutionStart {
        /// Tool call id.
        tool_call_id: String,
        /// Tool name.
        tool_name: String,
        /// Arguments.
        args: Value,
    },
    /// Tool streams progress.
    ToolExecutionUpdate {
        /// Tool call id.
        tool_call_id: String,
        /// Tool name.
        tool_name: String,
        /// Arguments.
        args: Value,
        /// Partial result.
        partial_result: AgentToolResult,
    },
    /// Tool completes.
    ToolExecutionEnd {
        /// Tool call id.
        tool_call_id: String,
        /// Tool name.
        tool_name: String,
        /// Result.
        result: AgentToolResult,
        /// Whether it failed.
        is_error: bool,
    },
}

impl AgentEvent {
    /// Event type name.
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::AgentStart => "agent_start",
            Self::AgentEnd { .. } => "agent_end",
            Self::Progress { .. } => "progress",
            Self::TurnStart => "turn_start",
            Self::TurnEnd { .. } => "turn_end",
            Self::MessageStart { .. } => "message_start",
            Self::MessageUpdate { .. } => "message_update",
            Self::MessageEnd { .. } => "message_end",
            Self::ToolExecutionStart { .. } => "tool_execution_start",
            Self::ToolExecutionUpdate { .. } => "tool_execution_update",
            Self::ToolExecutionEnd { .. } => "tool_execution_end",
        }
    }

    /// Whether this is the terminal agent_end event.
    pub fn is_agent_end(&self) -> bool {
        matches!(self, Self::AgentEnd { .. })
    }
}

/// Awaited event sink used by `run_agent_loop`.
pub type AgentEventSink =
    Arc<dyn Fn(AgentEvent) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// Convert AgentMessage[] to LLM Message[] (must not fail hard — return safe fallback).
pub type ConvertToLlmFn = Arc<
    dyn Fn(Vec<AgentMessage>) -> Pin<Box<dyn Future<Output = Vec<Message>> + Send>> + Send + Sync,
>;

/// Optional context transform before convert_to_llm.
pub type TransformContextFn = Arc<
    dyn Fn(
            Vec<AgentMessage>,
            Option<CancellationToken>,
        ) -> Pin<Box<dyn Future<Output = Vec<AgentMessage>> + Send>>
        + Send
        + Sync,
>;

/// Dynamic API key resolution by provider name.
pub type GetApiKeyFn =
    Arc<dyn Fn(String) -> Pin<Box<dyn Future<Output = Option<String>> + Send>> + Send + Sync>;

/// Predicate deciding whether to stop after a turn.
pub type ShouldStopAfterTurnFn = Arc<
    dyn Fn(ShouldStopAfterTurnContext) -> Pin<Box<dyn Future<Output = bool> + Send>> + Send + Sync,
>;

/// Produces an optional update applied before the next turn.
pub type PrepareNextTurnFn = Arc<
    dyn Fn(
            ShouldStopAfterTurnContext,
        ) -> Pin<Box<dyn Future<Output = Option<AgentLoopTurnUpdate>> + Send>>
        + Send
        + Sync,
>;

/// Async getter for queued messages (steering, follow-up).
pub type AgentMessagesFn =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Vec<AgentMessage>> + Send>> + Send + Sync>;

/// Hook run before a tool call executes.
pub type BeforeToolCallFn = Arc<
    dyn Fn(
            BeforeToolCallContext,
            Option<CancellationToken>,
        ) -> Pin<Box<dyn Future<Output = Option<BeforeToolCallResult>> + Send>>
        + Send
        + Sync,
>;

/// Hook run after a tool call executes.
pub type AfterToolCallFn = Arc<
    dyn Fn(
            AfterToolCallContext,
            Option<CancellationToken>,
        ) -> Pin<Box<dyn Future<Output = Option<AfterToolCallResult>> + Send>>
        + Send
        + Sync,
>;

/// Configuration for the agent loop.
#[derive(Clone)]
pub struct AgentLoopConfig {
    /// Model for provider requests.
    pub model: Model,
    /// Convert agent messages to LLM messages.
    pub convert_to_llm: ConvertToLlmFn,
    /// Optional context transform.
    pub transform_context: Option<TransformContextFn>,
    /// Dynamic API key resolution.
    pub get_api_key: Option<GetApiKeyFn>,
    /// Stop after turn predicate.
    pub should_stop_after_turn: Option<ShouldStopAfterTurnFn>,
    /// Prepare next turn snapshot.
    pub prepare_next_turn: Option<PrepareNextTurnFn>,
    /// Steering message getter.
    pub get_steering_messages: Option<AgentMessagesFn>,
    /// Follow-up message getter.
    pub get_follow_up_messages: Option<AgentMessagesFn>,
    /// Tool execution mode.
    pub tool_execution: ToolExecutionMode,
    /// Before tool call hook.
    pub before_tool_call: Option<BeforeToolCallFn>,
    /// After tool call hook.
    pub after_tool_call: Option<AfterToolCallFn>,
    /// Stream options passthrough (session_id, headers, reasoning, etc.).
    pub stream_options: SimpleStreamOptions,
    /// Thinking budgets.
    pub thinking_budgets: Option<ThinkingBudgets>,
}

impl AgentLoopConfig {
    /// Create a config with model and default convert_to_llm (LLM messages only).
    pub fn new(model: Model) -> Self {
        Self {
            model,
            convert_to_llm: Arc::new(|messages| {
                Box::pin(async move { default_convert_to_llm(&messages) })
            }),
            transform_context: None,
            get_api_key: None,
            should_stop_after_turn: None,
            prepare_next_turn: None,
            get_steering_messages: None,
            get_follow_up_messages: None,
            tool_execution: ToolExecutionMode::Parallel,
            before_tool_call: None,
            after_tool_call: None,
            stream_options: SimpleStreamOptions::default(),
            thinking_budgets: None,
        }
    }
}

/// Default converter: keep only LLM messages.
pub fn default_convert_to_llm(messages: &[AgentMessage]) -> Vec<Message> {
    messages
        .iter()
        .filter_map(|m| match m {
            AgentMessage::Llm(msg) => Some(msg.clone()),
            AgentMessage::Custom(_) => None,
        })
        .collect()
}

/// Public agent state.
#[derive(Clone)]
pub struct AgentState {
    /// System prompt.
    pub system_prompt: String,
    /// Active model.
    pub model: Model,
    /// Thinking level.
    pub thinking_level: AgentThinkingLevel,
    tools: Vec<AgentTool>,
    messages: Vec<AgentMessage>,
    /// True while processing (including awaited agent_end listeners).
    pub is_streaming: bool,
    /// Partial assistant message while streaming.
    pub streaming_message: Option<AgentMessage>,
    /// Tool call ids currently executing.
    pub pending_tool_calls: HashSet<String>,
    /// Error from most recent failed/aborted turn.
    pub error_message: Option<String>,
}

impl AgentState {
    /// Create state with model.
    pub fn new(model: Model) -> Self {
        Self {
            system_prompt: String::new(),
            model,
            thinking_level: AgentThinkingLevel::Off,
            tools: Vec::new(),
            messages: Vec::new(),
            is_streaming: false,
            streaming_message: None,
            pending_tool_calls: HashSet::new(),
            error_message: None,
        }
    }

    /// Tools (cloned).
    pub fn tools(&self) -> &[AgentTool] {
        &self.tools
    }

    /// Set tools (copies the top-level vec).
    pub fn set_tools(&mut self, tools: Vec<AgentTool>) {
        self.tools = tools;
    }

    /// Messages.
    pub fn messages(&self) -> &[AgentMessage] {
        &self.messages
    }

    /// Set messages (copies the top-level vec).
    pub fn set_messages(&mut self, messages: Vec<AgentMessage>) {
        self.messages = messages;
    }

    /// Mutable messages for push (direct mutation of current state).
    pub fn messages_mut(&mut self) -> &mut Vec<AgentMessage> {
        &mut self.messages
    }
}

/// Prompt input variants.
#[allow(
    clippy::large_enum_variant,
    reason = "public serde type; boxing variants would break the API"
)]
pub enum PromptInput {
    /// Plain text.
    Text(String),
    /// Text with images.
    TextWithImages {
        /// Prompt text.
        text: String,
        /// Image blocks.
        images: Vec<ImageContent>,
    },
    /// Single agent message.
    Message(AgentMessage),
    /// Multiple messages.
    Messages(Vec<AgentMessage>),
}

impl From<&str> for PromptInput {
    fn from(value: &str) -> Self {
        Self::Text(value.to_string())
    }
}

impl From<String> for PromptInput {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}

impl From<AgentMessage> for PromptInput {
    fn from(value: AgentMessage) -> Self {
        Self::Message(value)
    }
}
