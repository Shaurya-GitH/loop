//! The interface every agent backend implements, and the optional typed
//! services a backend may offer.
//!
//! A surface talks only to [`AgentBackend`]. Operations that not every
//! backend supports live in separate service traits, reached through
//! accessor methods that return `None` by default, so a surface cannot call an
//! operation the backend lacks.

use std::fmt;
use std::path::PathBuf;

use async_trait::async_trait;
use tokio::sync::{broadcast, oneshot};

use crate::capabilities::Capabilities;
use crate::content::ContentBlock;
use crate::error::{ErrorCategory, ErrorInfo};
use crate::ids::{BackendInstanceId, SessionKey};
use crate::interaction::{InteractionOutcome, InteractionRequest};
use crate::update::{SessionUpdate, StopReason, UpdateEnvelope};

/// Result of a backend operation.
pub type BackendResult<T> = Result<T, BackendError>;

/// Why a backend operation could not start.
///
/// Failures *during* a turn are reported through [`TurnOutcome`] and the
/// update stream, not here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendError {
    /// The session already has a turn running.
    Busy,
    /// The backend does not support this operation or input.
    Unsupported(String),
    /// The session does not exist or belongs to another backend.
    SessionNotFound(SessionKey),
    /// The operation failed.
    Failed(ErrorInfo),
}

impl BackendError {
    /// A failure with a category.
    pub fn failed(category: ErrorCategory, message: impl Into<String>) -> Self {
        Self::Failed(ErrorInfo::new(category, message))
    }
}

impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy => f.write_str("a turn is already running"),
            Self::Unsupported(what) => write!(f, "not supported by this agent: {what}"),
            Self::SessionNotFound(key) => write!(f, "session not found: {key}"),
            Self::Failed(info) => f.write_str(&info.message),
        }
    }
}

impl std::error::Error for BackendError {}

/// How to open a session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionSpec {
    /// Absolute working directory for the session.
    pub cwd: Option<PathBuf>,
    /// Display name.
    pub name: Option<String>,
}

/// A session as both Loop and the backend know it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionRef {
    /// Backend instance that owns the session.
    pub instance: BackendInstanceId,
    /// Loop's key for the session.
    pub key: SessionKey,
    /// The backend's own id for the session.
    pub native_id: String,
}

/// How a turn ended. Every turn has exactly one outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnOutcome {
    /// The turn finished.
    Completed(StopReason),
    /// The turn was cancelled.
    Cancelled,
    /// The turn failed.
    Failed(ErrorInfo),
    /// The backend was lost mid-turn. Edits or commands may already have run,
    /// so the prompt must not be re-sent automatically.
    Uncertain,
}

/// A running turn. Await [`TurnHandle::outcome`] to learn how it ended.
#[derive(Debug)]
pub struct TurnHandle {
    outcome: oneshot::Receiver<TurnOutcome>,
}

impl TurnHandle {
    /// A handle and the sender the backend completes it with.
    pub fn channel() -> (oneshot::Sender<TurnOutcome>, Self) {
        let (tx, rx) = oneshot::channel();
        (tx, Self { outcome: rx })
    }

    /// Wait for the turn to end. A backend that drops the turn without an
    /// outcome reports [`TurnOutcome::Uncertain`].
    pub async fn outcome(self) -> TurnOutcome {
        self.outcome.await.unwrap_or(TurnOutcome::Uncertain)
    }
}

/// An agent that runs turns: Loop's own agent loop, or an external agent.
#[async_trait]
pub trait AgentBackend: Send + Sync {
    /// The backend instance.
    fn instance(&self) -> &BackendInstanceId;

    /// What the backend supports.
    fn capabilities(&self) -> Capabilities;

    /// All updates from every session of this backend. Subscribe before
    /// starting work so no update is missed; route by
    /// [`UpdateEnvelope::session`].
    fn subscribe(&self) -> broadcast::Receiver<UpdateEnvelope>;

    /// Open a new session.
    async fn new_session(&self, spec: SessionSpec) -> BackendResult<SessionRef>;

    /// Reopen a session. History arrives on the update stream as replay.
    async fn load_session(
        &self,
        session: &SessionRef,
        spec: SessionSpec,
    ) -> BackendResult<SessionRef>;

    /// The session's history as updates, oldest first, for rebuilding a
    /// display. Backends that can only replay history while reopening a
    /// session return [`BackendError::Unsupported`].
    async fn transcript(&self, session: &SessionRef) -> BackendResult<Vec<SessionUpdate>> {
        let _ = session;
        Err(BackendError::Unsupported("reading the transcript".into()))
    }

    /// Start a turn. Updates arrive on the stream; the handle resolves once.
    async fn prompt(
        &self,
        session: &SessionRef,
        input: Vec<ContentBlock>,
    ) -> BackendResult<TurnHandle>;

    /// Ask the running turn to stop. Returns immediately; the turn's handle
    /// still resolves.
    fn cancel(&self, session: &SessionRef);

    /// Whether the session has a turn running.
    fn is_busy(&self, session: &SessionRef) -> bool;

    /// Stop all work and release the backend's resources.
    async fn shutdown(&self);

    /// Conversation compaction, when the backend lets Loop trigger it.
    fn compaction(&self) -> Option<&dyn Compaction> {
        None
    }

    /// Token and session statistics.
    fn session_stats(&self) -> Option<&dyn SessionStats> {
        None
    }

    /// Running a command for the user (`!command`) in the session's environment.
    fn local_shell(&self) -> Option<&dyn LocalShell> {
        None
    }
}

/// What Loop provides to every backend. Implemented once by the host
/// application; backends call it instead of talking to a surface directly.
#[async_trait]
pub trait HostServices: Send + Sync {
    /// Ask the user and wait for the answer. A host that cannot present the
    /// request returns [`InteractionOutcome::Rejected`]. Callers that can be
    /// cancelled race this against their cancellation and drop it.
    async fn interact(&self, request: InteractionRequest) -> InteractionOutcome;
}

/// Result of compacting a conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionReport {
    /// Estimated tokens before compaction, when known.
    pub tokens_before: Option<u64>,
    /// The summary that replaced the earlier conversation.
    pub summary: String,
}

/// Shrink a session's context by summarizing it.
#[async_trait]
pub trait Compaction: Send + Sync {
    /// Compact now, optionally guided by `instructions`.
    async fn compact(
        &self,
        session: &SessionRef,
        instructions: Option<&str>,
    ) -> BackendResult<CompactionReport>;
}

/// Token counts and a readable report for a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatsSummary {
    /// All tokens billed in the session.
    pub total_tokens: u64,
    /// Tokens in the current context, when known.
    pub context_tokens: Option<u64>,
    /// The model's context window, when known.
    pub context_window: Option<u64>,
    /// Multi-line report for display.
    pub report: String,
}

/// Session statistics.
#[async_trait]
pub trait SessionStats: Send + Sync {
    /// Statistics for the session.
    async fn stats(&self, session: &SessionRef) -> BackendResult<StatsSummary>;
}

/// Output of a command run for the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellResult {
    /// Standard output.
    pub stdout: String,
    /// Standard error.
    pub stderr: String,
    /// Exit code.
    pub exit_code: i32,
}

/// Run a command the user typed, in the session's execution environment.
#[async_trait]
pub trait LocalShell: Send + Sync {
    /// Run `command` and wait for it to finish.
    async fn run(&self, session: &SessionRef, command: &str) -> BackendResult<ShellResult>;
}
