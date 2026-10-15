//! Versioned event model shared by every Loop agent backend and surface.
//!
//! Backends (Loop's own agent, or an external agent over the Agent Client
//! Protocol) report each turn as a stream of [`UpdateEnvelope`]s. Surfaces
//! render only these events. See the crate README for the wire format.

#![deny(missing_docs)]

pub mod backend;
mod capabilities;
mod content;
mod error;
mod ids;
mod interaction;
mod update;

pub use backend::{
    AgentBackend, BackendError, BackendResult, Compaction, CompactionReport, HostFileSystem,
    HostServices, HostTerminals, LocalShell, ModelChoice, ModelChoices, ModelControl, SessionRef,
    SessionSpec, SessionStats, ShellResult, StatsSummary, TerminalExit, TerminalOutput,
    TerminalRequest, TurnHandle, TurnOutcome,
};
pub use capabilities::Capabilities;
pub use content::ContentBlock;
pub use error::{ErrorCategory, ErrorInfo};
pub use ids::{BackendInstanceId, InteractionId, MessageId, SessionKey, ToolCallId, TurnId};
pub use interaction::{
    Choice, InteractionOutcome, InteractionRequest, PermissionKind, PermissionOption,
};
pub use update::{
    NoticeLevel, Origin, PlanEntry, PlanEntryStatus, Role, SessionUpdate, StopReason, ToolCall,
    ToolCallStatus, ToolCallUpdate, ToolContent, ToolKind, ToolLocation, UpdateEnvelope, Usage,
};

/// Version of the event model. Bumped only for changes an older reader cannot
/// tolerate; additive changes keep the version.
pub const PROTOCOL_VERSION: u32 = 1;
