//! [`NativeBackend`]: Loop's own agent loop behind the
//! [`loop_protocol::AgentBackend`] interface.
//!
//! It wraps one [`AgentHarness`], which holds one active session at a time.
//! Agent events are translated by [`NativeEventMapper`] and published as
//! [`UpdateEnvelope`]s on a broadcast stream.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use loop_ai::ImageContent;
use loop_protocol::{
    AgentBackend, BackendError, BackendInstanceId, BackendResult, Capabilities, Compaction,
    CompactionReport, ContentBlock, ErrorCategory, ErrorInfo, LocalShell, SessionKey, SessionRef,
    SessionSpec, SessionStats, SessionUpdate, ShellResult, StatsSummary, StopReason, TurnHandle,
    TurnOutcome, UpdateEnvelope,
};
use parking_lot::Mutex;
use tokio::sync::broadcast;

use crate::harness::agent_harness::AgentHarness;
use crate::harness::session::format_session_stats;
use crate::harness::types::{AgentHarnessError, AgentHarnessPhase, ShellExecOptions};
use crate::protocol_events::{turn_outcome, NativeEventMapper};
use crate::types::{AgentEvent, PromptInput};

/// Instance id of Loop's own agent.
pub const NATIVE_INSTANCE: &str = "loop";

/// Updates buffered per subscriber before a slow reader starts losing the oldest.
const UPDATE_BUFFER: usize = 4096;

/// Loop's own agent loop as an [`AgentBackend`].
pub struct NativeBackend {
    instance: BackendInstanceId,
    harness: Arc<AgentHarness>,
    updates: broadcast::Sender<UpdateEnvelope>,
    stream: Arc<Mutex<StreamState>>,
    /// Set while a prompt sent through this backend runs, so two callers
    /// cannot both start one.
    prompting: Arc<AtomicBool>,
}

/// Clears [`NativeBackend::prompting`] when the prompt is over.
struct PromptSlot(Arc<AtomicBool>);

impl Drop for PromptSlot {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Translation state, shared with the harness subscription.
struct StreamState {
    session: SessionKey,
    seq: u64,
    mapper: NativeEventMapper,
}

impl NativeBackend {
    /// Wrap `harness`. Subscribes to its events immediately, so call this
    /// before any turn runs. Session changes must go through this backend
    /// (`new_session`, `load_session`, [`NativeBackend::adopt_session`]) so
    /// updates carry the right session key.
    pub async fn new(harness: Arc<AgentHarness>) -> Arc<Self> {
        let (updates, _) = broadcast::channel(UPDATE_BUFFER);
        let session = SessionKey(harness.session_id().await);
        let stream = Arc::new(Mutex::new(StreamState {
            session,
            seq: 0,
            mapper: NativeEventMapper::new(),
        }));
        let backend = Arc::new(Self {
            instance: BackendInstanceId::from(NATIVE_INSTANCE),
            harness: Arc::clone(&harness),
            updates: updates.clone(),
            stream: Arc::clone(&stream),
            prompting: Arc::new(AtomicBool::new(false)),
        });
        let instance = backend.instance.clone();
        harness.subscribe(move |event: AgentEvent| {
            let envelopes = translate(&instance, &stream, &event);
            for envelope in envelopes {
                // No receivers is fine: nobody is listening yet.
                let _ = updates.send(envelope);
            }
            async {}
        });
        backend
    }

    /// The wrapped harness, for native-only features (sandbox, skills, MCP,
    /// forking, workflows).
    pub fn harness(&self) -> &Arc<AgentHarness> {
        &self.harness
    }

    /// A reference to the harness's current session.
    pub async fn current_session(&self) -> SessionRef {
        self.session_ref(self.harness.session_id().await)
    }

    /// Point the update stream at a session the harness switched to by
    /// itself (for example after forking).
    pub fn adopt_session(&self, native_id: &str) -> SessionRef {
        let mut stream = self.stream.lock();
        stream.session = SessionKey::from(native_id);
        stream.seq = 0;
        stream.mapper = NativeEventMapper::new();
        drop(stream);
        self.session_ref(native_id.to_owned())
    }

    fn session_ref(&self, native_id: String) -> SessionRef {
        SessionRef {
            instance: self.instance.clone(),
            key: SessionKey(native_id.clone()),
            native_id,
        }
    }

    async fn check_session(&self, session: &SessionRef) -> BackendResult<()> {
        if session.instance != self.instance || session.native_id != self.harness.session_id().await
        {
            return Err(BackendError::SessionNotFound(session.key.clone()));
        }
        Ok(())
    }
}

fn translate(
    instance: &BackendInstanceId,
    stream: &Mutex<StreamState>,
    event: &AgentEvent,
) -> Vec<UpdateEnvelope> {
    let mut state = stream.lock();
    let updates = state.mapper.map(event);
    let mut envelopes = Vec::with_capacity(updates.len());
    for update in updates {
        let turn = match &update {
            SessionUpdate::TurnStarted { turn_id } | SessionUpdate::TurnEnded { turn_id, .. } => {
                Some(turn_id.clone())
            }
            _ => state.mapper.current_turn().cloned(),
        };
        state.seq += 1;
        envelopes.push(UpdateEnvelope::live(
            instance.clone(),
            0,
            state.session.clone(),
            turn,
            state.seq,
            update,
        ));
    }
    envelopes
}

fn harness_error(err: AgentHarnessError) -> BackendError {
    match err {
        AgentHarnessError::Busy => BackendError::Busy,
        AgentHarnessError::NoModelSelected => {
            BackendError::failed(ErrorCategory::Auth, err.to_string())
        }
        other => BackendError::failed(ErrorCategory::Internal, other.to_string()),
    }
}

fn prompt_input(input: Vec<ContentBlock>) -> BackendResult<PromptInput> {
    let mut text = String::new();
    let mut images = Vec::new();
    for block in input {
        match block {
            ContentBlock::Text { text: t } => {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&t);
            }
            ContentBlock::Image { data, mime_type } => {
                images.push(ImageContent { data, mime_type })
            }
            other => {
                return Err(BackendError::Unsupported(format!(
                    "prompt content {}",
                    serde_json::to_value(&other)
                        .ok()
                        .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(str::to_owned))
                        .unwrap_or_else(|| "of unknown type".into())
                )))
            }
        }
    }
    Ok(if images.is_empty() {
        PromptInput::Text(text)
    } else {
        PromptInput::TextWithImages { text, images }
    })
}

#[async_trait]
impl AgentBackend for NativeBackend {
    fn instance(&self) -> &BackendInstanceId {
        &self.instance
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            load_session: true,
            set_model: true,
            usage: true,
            images: true,
            ..Capabilities::default()
        }
    }

    fn subscribe(&self) -> broadcast::Receiver<UpdateEnvelope> {
        self.updates.subscribe()
    }

    async fn new_session(&self, spec: SessionSpec) -> BackendResult<SessionRef> {
        let cwd = spec.cwd.map(|p| p.to_string_lossy().into_owned());
        let id = self
            .harness
            .start_new_session(cwd, spec.name)
            .await
            .map_err(harness_error)?;
        Ok(self.adopt_session(&id))
    }

    async fn load_session(
        &self,
        session: &SessionRef,
        _spec: SessionSpec,
    ) -> BackendResult<SessionRef> {
        if session.instance != self.instance {
            return Err(BackendError::SessionNotFound(session.key.clone()));
        }
        let id = self
            .harness
            .switch_session(&session.native_id)
            .await
            .map_err(harness_error)?;
        Ok(self.adopt_session(&id))
    }

    async fn prompt(
        &self,
        session: &SessionRef,
        input: Vec<ContentBlock>,
    ) -> BackendResult<TurnHandle> {
        self.check_session(session).await?;
        // Reserve the turn before checking the harness, so two callers
        // cannot both pass the check.
        if self
            .prompting
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(BackendError::Busy);
        }
        let slot = PromptSlot(Arc::clone(&self.prompting));
        if self.harness.phase() != AgentHarnessPhase::Idle {
            return Err(BackendError::Busy);
        }
        let input = prompt_input(input)?;
        let (done, handle) = TurnHandle::channel();
        let harness = Arc::clone(&self.harness);
        tokio::spawn(async move {
            let result = harness.prompt(input).await;
            drop(slot);
            let outcome = match result {
                Ok(message) => match message.as_assistant().map(turn_outcome) {
                    Some((StopReason::Cancelled, _)) => TurnOutcome::Cancelled,
                    Some((StopReason::Error, error)) => {
                        TurnOutcome::Failed(error.unwrap_or_else(|| {
                            ErrorInfo::new(ErrorCategory::Agent, "the turn failed")
                        }))
                    }
                    Some((stop, _)) => TurnOutcome::Completed(stop),
                    None => TurnOutcome::Completed(StopReason::EndTurn),
                },
                Err(AgentHarnessError::Busy) => TurnOutcome::Failed(ErrorInfo::new(
                    ErrorCategory::Internal,
                    "a turn is already running",
                )),
                Err(err) => TurnOutcome::Failed(match harness_error(err) {
                    BackendError::Failed(info) => info,
                    other => ErrorInfo::new(ErrorCategory::Internal, other.to_string()),
                }),
            };
            let _ = done.send(outcome);
        });
        Ok(handle)
    }

    fn cancel(&self, _session: &SessionRef) {
        self.harness.abort();
    }

    fn is_busy(&self, _session: &SessionRef) -> bool {
        self.prompting.load(Ordering::Acquire) || self.harness.phase() != AgentHarnessPhase::Idle
    }

    async fn shutdown(&self) {
        self.harness.request_shutdown();
    }

    fn compaction(&self) -> Option<&dyn Compaction> {
        Some(self)
    }

    fn session_stats(&self) -> Option<&dyn SessionStats> {
        Some(self)
    }

    fn local_shell(&self) -> Option<&dyn LocalShell> {
        Some(self)
    }
}

#[async_trait]
impl Compaction for NativeBackend {
    async fn compact(
        &self,
        session: &SessionRef,
        instructions: Option<&str>,
    ) -> BackendResult<CompactionReport> {
        self.check_session(session).await?;
        let result = self
            .harness
            .compact(instructions)
            .await
            .map_err(harness_error)?;
        Ok(CompactionReport {
            tokens_before: Some(result.tokens_before),
            summary: result.summary,
        })
    }
}

#[async_trait]
impl SessionStats for NativeBackend {
    async fn stats(&self, session: &SessionRef) -> BackendResult<StatsSummary> {
        self.check_session(session).await?;
        let stats = self.harness.session_stats().await.map_err(harness_error)?;
        Ok(StatsSummary {
            total_tokens: stats.tokens.total_tokens(),
            context_tokens: stats.context_usage.as_ref().and_then(|c| c.tokens),
            context_window: stats.context_usage.as_ref().map(|c| c.context_window),
            report: format_session_stats(&stats),
        })
    }
}

#[async_trait]
impl LocalShell for NativeBackend {
    async fn run(&self, session: &SessionRef, command: &str) -> BackendResult<ShellResult> {
        self.check_session(session).await?;
        let env = self.harness.tool_env().await.map_err(harness_error)?;
        let out = env
            .exec(command, ShellExecOptions::default())
            .await
            .map_err(|e| BackendError::failed(ErrorCategory::Agent, e.to_string()))?;
        Ok(ShellResult {
            stdout: out.stdout,
            stderr: out.stderr,
            exit_code: out.exit_code,
        })
    }
}
