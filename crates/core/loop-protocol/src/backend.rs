//! The interface every agent backend implements, and the optional typed
//! services a backend may offer.
//!
//! A surface talks only to [`AgentBackend`]. Operations that not every
//! backend supports live in separate service traits, reached through
//! accessor methods that return `None` by default, so a surface cannot call an
//! operation the backend lacks.

use std::fmt;
use std::path::{Path, PathBuf};

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

    /// Choosing the model the backend uses for a session.
    fn model_control(&self) -> Option<&dyn ModelControl> {
        None
    }
}

/// One model a backend offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelChoice {
    /// The backend's id for the model, passed back to [`ModelControl::set_model`].
    pub id: String,
    /// Display name.
    pub name: String,
    /// Short description, when the backend gives one.
    pub description: Option<String>,
}

/// The models a session can use, and the one it uses now.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelChoices {
    /// The current model's id, when known.
    pub current: Option<String>,
    /// Every model on offer.
    pub models: Vec<ModelChoice>,
}

/// Model selection for backends that choose their own models.
#[async_trait]
pub trait ModelControl: Send + Sync {
    /// The models the session can use.
    async fn models(&self, session: &SessionRef) -> BackendResult<ModelChoices>;
    /// Switch the session to `model` (a [`ModelChoice::id`]).
    async fn set_model(&self, session: &SessionRef, model: &str) -> BackendResult<()>;
    /// The reasoning effort levels the session's model offers, as choices.
    async fn efforts(&self, _session: &SessionRef) -> BackendResult<ModelChoices> {
        Err(BackendError::Unsupported(
            "choosing a reasoning effort".into(),
        ))
    }
    /// Switch the session to reasoning effort `effort` (a [`ModelChoice::id`]
    /// from [`Self::efforts`]).
    async fn set_effort(&self, _session: &SessionRef, _effort: &str) -> BackendResult<()> {
        Err(BackendError::Unsupported(
            "choosing a reasoning effort".into(),
        ))
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

    /// File access performed by Loop on the backend's behalf. When `None`,
    /// backends must not offer agents file access through Loop.
    fn file_system(&self) -> Option<&dyn HostFileSystem> {
        None
    }

    /// Commands run by Loop on the backend's behalf. When `None`, backends
    /// must not offer agents terminals through Loop.
    fn terminals(&self) -> Option<&dyn HostTerminals> {
        None
    }
}

/// Text file access through Loop, so its approval policy, review and
/// conflict checks apply to an agent's file changes.
#[async_trait]
pub trait HostFileSystem: Send + Sync {
    /// Read a text file, optionally from a 1-based `line` for `limit` lines.
    async fn read_text_file(
        &self,
        session: &SessionRef,
        path: &Path,
        line: Option<u32>,
        limit: Option<u32>,
    ) -> BackendResult<String>;

    /// Write a text file, creating it if needed.
    async fn write_text_file(
        &self,
        session: &SessionRef,
        path: &Path,
        content: &str,
    ) -> BackendResult<()>;
}

/// A command to run in a terminal Loop manages.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TerminalRequest {
    /// Program to run.
    pub command: String,
    /// Arguments.
    pub args: Vec<String>,
    /// Extra environment variables.
    pub env: Vec<(String, String)>,
    /// Working directory. Defaults to the session's.
    pub cwd: Option<PathBuf>,
    /// Keep at most this many bytes of output (the most recent ones).
    pub output_byte_limit: Option<u64>,
}

/// How a terminal's command ended.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TerminalExit {
    /// Exit code, when the process exited normally.
    pub exit_code: Option<u32>,
    /// Signal name, when the process was killed by one.
    pub signal: Option<String>,
}

/// Output of a terminal so far.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TerminalOutput {
    /// Output text.
    pub output: String,
    /// Whether earlier output was dropped to respect the byte limit.
    pub truncated: bool,
    /// Set once the command has ended.
    pub exit: Option<TerminalExit>,
}

/// Terminals run by Loop, so its sandbox and approval policy apply to an
/// agent's commands. Ids are the host's own.
#[async_trait]
pub trait HostTerminals: Send + Sync {
    /// Start a command. Returns the terminal id.
    async fn create(&self, session: &SessionRef, request: TerminalRequest)
        -> BackendResult<String>;
    /// Output so far, without waiting.
    async fn output(
        &self,
        session: &SessionRef,
        terminal_id: &str,
    ) -> BackendResult<TerminalOutput>;
    /// Wait for the command to end.
    async fn wait_for_exit(
        &self,
        session: &SessionRef,
        terminal_id: &str,
    ) -> BackendResult<TerminalExit>;
    /// Kill the command; the terminal stays readable until released.
    async fn kill(&self, session: &SessionRef, terminal_id: &str) -> BackendResult<()>;
    /// Kill the command if still running and forget the terminal.
    async fn release(&self, session: &SessionRef, terminal_id: &str) -> BackendResult<()>;
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
