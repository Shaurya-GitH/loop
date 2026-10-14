//! Session updates and the envelope that routes them.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::content::ContentBlock;
use crate::error::ErrorInfo;
use crate::ids::{BackendInstanceId, InteractionId, MessageId, SessionKey, ToolCallId, TurnId};
use crate::interaction::{InteractionOutcome, InteractionRequest};
use crate::PROTOCOL_VERSION;

/// An update together with everything needed to route it.
///
/// A backend publishes one stream of envelopes for all its sessions; readers
/// route by `session` and discard envelopes from an older `generation`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UpdateEnvelope {
    /// Protocol version of this envelope.
    pub v: u32,
    /// Backend instance that produced the update.
    pub instance: BackendInstanceId,
    /// Process generation. Increases each time the backend's process restarts.
    pub generation: u64,
    /// Session the update belongs to.
    pub session: SessionKey,
    /// Turn the update belongs to, when it belongs to one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<TurnId>,
    /// Position in the backend's stream for this session. Strictly increasing.
    pub seq: u64,
    /// Whether this is live activity or replayed history.
    pub origin: Origin,
    /// The update.
    pub update: SessionUpdate,
}

impl UpdateEnvelope {
    /// A live envelope at the current protocol version.
    pub fn live(
        instance: BackendInstanceId,
        generation: u64,
        session: SessionKey,
        turn: Option<TurnId>,
        seq: u64,
        update: SessionUpdate,
    ) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            instance,
            generation,
            session,
            turn,
            seq,
            origin: Origin::Live,
            update,
        }
    }
}

/// Where an update comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    /// Produced by the running session.
    Live,
    /// Replayed history from reopening a session. Rebuilds the display; never
    /// stored again.
    Replay,
}

/// One change in a session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionUpdate {
    /// A turn began.
    TurnStarted {
        /// Turn id.
        turn_id: TurnId,
    },
    /// A message began.
    MessageStarted {
        /// Message id.
        message_id: MessageId,
        /// Author.
        role: Role,
    },
    /// Part of a message.
    MessageChunk {
        /// Message id.
        message_id: MessageId,
        /// Author.
        role: Role,
        /// Content to append.
        content: ContentBlock,
    },
    /// A message is complete.
    MessageEnded {
        /// Message id.
        message_id: MessageId,
    },
    /// Part of the agent's reasoning.
    ThoughtChunk {
        /// Message the thought belongs to.
        message_id: MessageId,
        /// Content to append.
        content: ContentBlock,
    },
    /// A tool call began.
    ToolCall(ToolCall),
    /// A tool call changed. Only changed fields are set.
    ToolCallUpdate(ToolCallUpdate),
    /// The agent's plan, replacing any earlier plan.
    Plan {
        /// Plan entries in order.
        entries: Vec<PlanEntry>,
    },
    /// Token usage so far.
    Usage(Usage),
    /// Something to show that is not part of the conversation.
    Notice {
        /// Severity.
        level: NoticeLevel,
        /// Text to show.
        text: String,
    },
    /// The backend is waiting for the user.
    InteractionRequested {
        /// The request.
        request: InteractionRequest,
    },
    /// A pending interaction was answered or withdrawn.
    InteractionResolved {
        /// Interaction id.
        id: InteractionId,
        /// The answer.
        outcome: InteractionOutcome,
    },
    /// The turn is over. Emitted exactly once per turn.
    TurnEnded {
        /// Turn id.
        turn_id: TurnId,
        /// Why it ended.
        stop: StopReason,
        /// Failure details when `stop` is `Error`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<ErrorInfo>,
    },
    /// An update with a `type` this reader does not know. The raw JSON is kept.
    #[serde(untagged)]
    Unknown(Value),
}

/// Who wrote a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// The person using Loop.
    User,
    /// The agent.
    Agent,
}

/// Why a turn ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// The agent finished.
    EndTurn,
    /// The model hit its output limit.
    MaxTokens,
    /// The turn hit its request limit.
    MaxTurnRequests,
    /// The model refused.
    Refusal,
    /// The turn was cancelled.
    Cancelled,
    /// The turn failed. `TurnEnded::error` says why.
    Error,
    /// A reason this reader does not know.
    #[serde(other)]
    Other,
}

/// A tool call as first reported.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Tool call id.
    pub id: ToolCallId,
    /// The tool's own name, when the backend reports one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Short description to show, such as `edit src/main.rs`.
    pub title: String,
    /// What kind of action the tool performs.
    pub kind: ToolKind,
    /// Current status.
    pub status: ToolCallStatus,
    /// Output so far.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub content: Vec<ToolContent>,
    /// Files the call touches.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub locations: Vec<ToolLocation>,
    /// The tool's input, as the backend reported it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_input: Option<Value>,
}

/// Changed fields of a tool call. `content`, when set, replaces the earlier content.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallUpdate {
    /// Tool call id.
    pub id: ToolCallId,
    /// The tool's own name, when the backend reports one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// New title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// New kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<ToolKind>,
    /// New status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ToolCallStatus>,
    /// Replacement content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<Vec<ToolContent>>,
    /// Replacement locations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locations: Option<Vec<ToolLocation>>,
    /// The tool's raw output, as the backend reported it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_output: Option<Value>,
}

impl ToolCallUpdate {
    /// An update that changes nothing yet.
    pub fn new(id: ToolCallId) -> Self {
        Self {
            id,
            name: None,
            title: None,
            kind: None,
            status: None,
            content: None,
            locations: None,
            raw_output: None,
        }
    }
}

/// What kind of action a tool performs. Used for icons and approval policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    /// Reads files or data.
    Read,
    /// Changes files.
    Edit,
    /// Deletes files.
    Delete,
    /// Moves or renames files.
    Move,
    /// Searches files or data.
    Search,
    /// Runs a command.
    Execute,
    /// Fetches from the network.
    Fetch,
    /// Internal reasoning step.
    Think,
    /// Anything else, including kinds this reader does not know.
    #[serde(other)]
    Other,
}

/// Progress of a tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallStatus {
    /// Waiting to start, for example for approval.
    Pending,
    /// Running.
    InProgress,
    /// Finished successfully.
    Completed,
    /// Failed or was rejected.
    Failed,
}

/// Output of a tool call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolContent {
    /// Ordinary content: text, an image or a resource link.
    Content {
        /// The content.
        content: ContentBlock,
    },
    /// A file change as full before and after text.
    Diff {
        /// File path.
        path: String,
        /// Previous text. `None` for a new file.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        old_text: Option<String>,
        /// New text.
        new_text: String,
    },
    /// A file change as a unified diff.
    UnifiedDiff {
        /// File path.
        path: String,
        /// Unified diff text.
        diff: String,
    },
    /// Live output of a terminal Loop runs for the backend.
    Terminal {
        /// Terminal id.
        terminal_id: String,
    },
    /// Tool content with a `type` this reader does not know. The raw JSON is kept.
    #[serde(untagged)]
    Unknown(Value),
}

impl ToolContent {
    /// Text tool content.
    pub fn text(text: impl Into<String>) -> Self {
        Self::Content {
            content: ContentBlock::text(text),
        }
    }
}

/// A file location a tool call touches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolLocation {
    /// File path.
    pub path: String,
    /// 1-based line, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
}

/// One step of the agent's plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanEntry {
    /// What the step does.
    pub content: String,
    /// Progress.
    pub status: PlanEntryStatus,
}

/// Progress of a plan step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanEntryStatus {
    /// Not started.
    Pending,
    /// Under way.
    InProgress,
    /// Done.
    Completed,
    /// A status this reader does not know.
    #[serde(other)]
    Other,
}

/// Token usage reported by a backend.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    /// Prompt tokens of the latest request.
    pub input_tokens: u64,
    /// Output tokens of the latest request.
    pub output_tokens: u64,
    /// Prompt tokens served from cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input_tokens: Option<u64>,
    /// Prompt tokens written to cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u64>,
    /// Reasoning tokens, when reported separately.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
    /// Tokens in the context after the latest request.
    pub total_tokens: u64,
    /// The model's context window, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    /// Cost of the latest request in US dollars, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
}

/// Severity of a notice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeLevel {
    /// Status or progress.
    Info,
    /// Something the user may want to act on.
    Warning,
    /// Something went wrong but the turn continues.
    Error,
    /// A level this reader does not know.
    #[serde(other)]
    Other,
}
