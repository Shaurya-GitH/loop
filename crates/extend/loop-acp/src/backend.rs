//! [`AcpBackend`]: an external ACP agent behind [`AgentBackend`].
//!
//! The backend owns the connection → session → turn → pending-request state
//! machine:
//! - Each process start is a new *generation*. Envelopes carry it; answers
//!   for an older generation are never sent.
//! - Every prompt ends with exactly one `TurnEnded` and one [`TurnOutcome`].
//! - When the agent process is lost mid-turn, every live turn in it ends as
//!   [`TurnOutcome::Uncertain`] and the prompt is never re-sent.
//! - `cancel` is fire-and-forget (ACP `session/cancel` is a notification).
//!   Pending interactions are answered `cancelled`; a turn that does not end
//!   within the agent's cancel timeout gets its process killed.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use agent_client_protocol_schema::v1 as acp;
use agent_client_protocol_schema::ProtocolVersion;
use async_trait::async_trait;
use loop_protocol::{
    AgentBackend, BackendError, BackendInstanceId, BackendResult, Capabilities, ContentBlock,
    ErrorCategory, ErrorInfo, HostServices, InteractionId, InteractionOutcome, InteractionRequest,
    ModelChoice, ModelChoices, ModelControl, Origin, SessionKey, SessionRef, SessionSpec,
    SessionUpdate, StopReason, TerminalRequest, TurnHandle, TurnId, TurnOutcome, UpdateEnvelope,
    PROTOCOL_VERSION,
};
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::mapping::{
    acp_content, permission_options, remote_error, session_update, stop_reason, tool_call_update,
    MessageTracker,
};
use crate::process::{AgentConfig, AgentProcess, Connector};
use crate::rpc::{codes, Connection, Incoming, RpcError};

/// Updates buffered per subscriber before a slow reader starts losing the oldest.
const UPDATE_BUFFER: usize = 4096;

/// An external agent reached over ACP.
pub struct AcpBackend {
    me: Weak<AcpBackend>,
    config: AgentConfig,
    cwd: PathBuf,
    connector: Arc<dyn Connector>,
    host: Arc<dyn HostServices>,
    updates: broadcast::Sender<UpdateEnvelope>,
    state: Mutex<State>,
    link: tokio::sync::Mutex<Option<Live>>,
}

impl std::fmt::Debug for AcpBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcpBackend")
            .field("agent", &self.config.id)
            .field("command", &self.config.command)
            .finish_non_exhaustive()
    }
}

/// The current agent process and connection.
struct Live {
    conn: Arc<Connection>,
    generation: u64,
    process: Option<AgentProcess>,
}

#[derive(Default)]
struct State {
    /// The live connection and its generation, for synchronous use.
    conn: Option<(u64, Arc<Connection>)>,
    generation: u64,
    next_turn: u64,
    capabilities: Capabilities,
    auth_methods: Vec<acp::AuthMethod>,
    /// Sessions by the agent's own id.
    sessions: HashMap<String, SessionState>,
}

struct SessionState {
    key: SessionKey,
    /// Working directory the session was opened with, reused on reopen.
    cwd: PathBuf,
    /// The agent's configuration options for the session (model and others).
    config_options: Vec<acp::SessionConfigOption>,
    /// Generation of the process that holds this session, or `None` after
    /// that process was lost.
    attached: Option<u64>,
    seq: u64,
    messages: MessageTracker,
    /// Whether updates arriving now are replayed history.
    replay: Replay,
    turn: Option<ActiveTurn>,
}

/// How to treat history the agent replays while a session is reopened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Replay {
    /// Not reopening: updates are live.
    Off,
    /// Reopened at the user's request: publish history as replay.
    Show,
    /// Reattached after a restart: the surface already shows this history.
    Hide,
}

struct ActiveTurn {
    id: TurnId,
    generation: u64,
    done: oneshot::Sender<TurnOutcome>,
    /// Cancelled when the turn is cancelled or ends; pending interactions
    /// race against it.
    cancel: CancellationToken,
}

impl AcpBackend {
    /// Start the agent and initialize the connection. `cwd` is the agent
    /// process's working directory and the default session directory.
    pub async fn start(
        config: AgentConfig,
        cwd: PathBuf,
        connector: Arc<dyn Connector>,
        host: Arc<dyn HostServices>,
    ) -> BackendResult<Arc<Self>> {
        let (updates, _) = broadcast::channel(UPDATE_BUFFER);
        let backend = Arc::new_cyclic(|me| Self {
            me: me.clone(),
            config,
            cwd,
            connector,
            host,
            updates,
            state: Mutex::new(State::default()),
            link: tokio::sync::Mutex::new(None),
        });
        backend.connection().await?;
        Ok(backend)
    }

    /// The agent's sign-in methods, as it advertised them.
    pub fn auth_methods(&self) -> Vec<acp::AuthMethod> {
        self.lock_state().auth_methods.clone()
    }

    /// Sign in with an agent-handled method (ACP `authenticate`). Terminal
    /// methods are run by the caller, which then calls [`AcpBackend::restart`].
    pub async fn authenticate(&self, method_id: &str) -> BackendResult<()> {
        let conn = self.connection().await?;
        let request = acp::AuthenticateRequest::new(acp::AuthMethodId::new(method_id));
        conn.request("authenticate", to_value(&request)?)
            .await
            .map_err(rpc_failure)?;
        Ok(())
    }

    /// Stop the agent process and start a fresh one, for example after a
    /// terminal sign-in. Live turns end as uncertain.
    pub async fn restart(&self) -> BackendResult<()> {
        self.kill().await;
        self.connection().await.map(|_| ())
    }

    fn lock_state(&self) -> MutexGuard<'_, State> {
        // A panic while holding the lock leaves plain data; keep going.
        self.state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// The live connection, starting the agent if needed.
    async fn connection(&self) -> BackendResult<Arc<Connection>> {
        let mut link = self.link.lock().await;
        if let Some(live) = link.as_ref() {
            return Ok(Arc::clone(&live.conn));
        }
        let generation = {
            let mut state = self.lock_state();
            state.generation += 1;
            state.generation
        };
        let agent = self
            .connector
            .connect(&self.config, &self.cwd)
            .await
            .map_err(|e| {
                BackendError::failed(
                    ErrorCategory::Agent,
                    format!("could not start {}: {e}", self.config.name),
                )
            })?;
        let (conn, incoming) = Connection::new(agent.reader, agent.writer);
        let mut live = Live {
            conn: Arc::clone(&conn),
            generation,
            process: agent.process,
        };
        tokio::spawn(dispatch(
            self.me.clone(),
            generation,
            Arc::clone(&conn),
            incoming,
        ));

        let fs = self.host.file_system().is_some();
        let request = acp::InitializeRequest::new(ProtocolVersion::V1)
            .client_capabilities(
                acp::ClientCapabilities::new()
                    .fs(acp::FileSystemCapabilities::new()
                        .read_text_file(fs)
                        .write_text_file(fs))
                    .terminal(self.host.terminals().is_some()),
            )
            .client_info(acp::Implementation::new("loop", env!("CARGO_PKG_VERSION")));
        let handshake = tokio::time::timeout(
            self.config.start_timeout,
            conn.request("initialize", to_value(&request)?),
        )
        .await
        .unwrap_or_else(|_| {
            Err(RpcError::Invalid(format!(
                "{} did not answer within {}s",
                self.config.name,
                self.config.start_timeout.as_secs()
            )))
        });
        let response = match handshake {
            Ok(response) => response,
            Err(error) => {
                if let Some(process) = live.process.as_mut() {
                    process.kill_tree().await;
                }
                conn.close();
                return Err(rpc_failure(error));
            }
        };
        let response: acp::InitializeResponse = serde_json::from_value(response)
            .map_err(|e| BackendError::failed(ErrorCategory::Protocol, e.to_string()))?;
        if response.protocol_version != ProtocolVersion::V1 {
            conn.close();
            return Err(BackendError::failed(
                ErrorCategory::Protocol,
                format!(
                    "{} speaks ACP version {:?}; Loop supports version 1",
                    self.config.name, response.protocol_version
                ),
            ));
        }
        {
            let mut state = self.lock_state();
            let agent = &response.agent_capabilities;
            state.capabilities = Capabilities {
                load_session: agent.load_session,
                resume_session: agent.session_capabilities.resume.is_some(),
                images: agent.prompt_capabilities.image,
                usage: true,
                fs_client: fs,
                terminal_client: self.host.terminals().is_some(),
                ..Capabilities::default()
            };
            state.auth_methods = response.auth_methods;
            state.conn = Some((generation, Arc::clone(&conn)));
        }
        *link = Some(live);
        Ok(conn)
    }

    /// Kill the agent process and close its connection. The dispatcher then
    /// sees the connection end and settles live turns as uncertain.
    async fn kill(&self) {
        self.lock_state().conn = None;
        let live = self.link.lock().await.take();
        if let Some(mut live) = live {
            if let Some(process) = live.process.as_mut() {
                process.kill_tree().await;
            }
            live.conn.close();
        }
    }

    /// The process for `generation` ended: end its live turns as uncertain and
    /// detach its sessions.
    async fn connection_lost(&self, generation: u64) {
        {
            let mut link = self.link.lock().await;
            if link
                .as_ref()
                .is_some_and(|live| live.generation == generation)
            {
                *link = None;
            }
        }
        let mut state = self.lock_state();
        if state.conn.as_ref().is_some_and(|(g, _)| *g == generation) {
            state.conn = None;
        }
        let instance = self.config.id.clone();
        let mut envelopes = Vec::new();
        for session in state.sessions.values_mut() {
            if session.attached == Some(generation) {
                session.attached = None;
            }
            if session
                .turn
                .as_ref()
                .is_some_and(|turn| turn.generation == generation)
            {
                let info = ErrorInfo::new(
                    ErrorCategory::Agent,
                    "the agent stopped during this turn; its changes may be incomplete",
                );
                envelopes.extend(end_turn(
                    &instance,
                    generation,
                    session,
                    StopReason::Error,
                    Some(info),
                    TurnOutcome::Uncertain,
                ));
            }
        }
        drop(state);
        for envelope in envelopes {
            let _ = self.updates.send(envelope);
        }
    }

    /// Publish `updates` for the session with agent id `native`.
    fn publish(&self, native: &str, generation: u64, updates: Vec<SessionUpdate>) {
        let mut state = self.lock_state();
        let instance = self.config.id.clone();
        let Some(session) = state.sessions.get_mut(native) else {
            tracing::debug!(session = native, "update for a session Loop does not know");
            return;
        };
        let envelopes: Vec<_> = updates
            .into_iter()
            .map(|update| envelope(&instance, generation, session, update))
            .collect();
        drop(state);
        for envelope in envelopes {
            let _ = self.updates.send(envelope);
        }
    }

    /// End the session's turn `turn_id` if it is still the live one.
    fn finish_turn(
        &self,
        native: &str,
        turn_id: &TurnId,
        stop: StopReason,
        error: Option<ErrorInfo>,
        outcome: TurnOutcome,
    ) {
        let mut state = self.lock_state();
        let instance = self.config.id.clone();
        let Some(session) = state.sessions.get_mut(native) else {
            return;
        };
        let Some(generation) = session
            .turn
            .as_ref()
            .filter(|turn| &turn.id == turn_id)
            .map(|turn| turn.generation)
        else {
            // Already settled, for example by connection loss.
            return;
        };
        let envelopes = end_turn(&instance, generation, session, stop, error, outcome);
        drop(state);
        for envelope in envelopes {
            let _ = self.updates.send(envelope);
        }
    }

    /// Make sure `session` is attached to the live process, reloading it after
    /// a restart when the agent supports that.
    /// Whether `session` names one of this backend's sessions. Sessions are
    /// looked up by native id, which another agent may reuse.
    fn owns(&self, session: &SessionRef) -> bool {
        session.instance == self.config.id
    }

    async fn attach(&self, session: &SessionRef) -> BackendResult<Arc<Connection>> {
        if !self.owns(session) {
            return Err(BackendError::SessionNotFound(session.key.clone()));
        }
        let conn = self.connection().await?;
        let (generation, attached, can_load, cwd) = {
            let state = self.lock_state();
            let Some(known) = state.sessions.get(&session.native_id) else {
                return Err(BackendError::SessionNotFound(session.key.clone()));
            };
            (
                state.generation,
                known.attached,
                state.capabilities.load_session,
                known.cwd.clone(),
            )
        };
        if attached == Some(generation) {
            return Ok(conn);
        }
        if !can_load {
            return Err(BackendError::failed(
                ErrorCategory::Agent,
                "the agent restarted and cannot reopen this session; start a new one",
            ));
        }
        // Reattach quietly, in the session's own directory: the surface
        // already shows this history.
        let spec = SessionSpec {
            cwd: Some(cwd),
            name: None,
        };
        self.load(&conn, session, &spec, false).await?;
        Ok(conn)
    }

    async fn load(
        &self,
        conn: &Connection,
        session: &SessionRef,
        spec: &SessionSpec,
        show_history: bool,
    ) -> BackendResult<()> {
        let generation = {
            let mut state = self.lock_state();
            let generation = state.generation;
            let cwd = self.session_cwd(spec);
            let entry = state
                .sessions
                .entry(session.native_id.clone())
                .or_insert_with(|| SessionState::new(session.key.clone(), cwd.clone()));
            entry.cwd = cwd;
            entry.replay = if show_history {
                Replay::Show
            } else {
                Replay::Hide
            };
            entry.messages = MessageTracker::default();
            generation
        };
        let request = acp::LoadSessionRequest::new(
            acp::SessionId::new(session.native_id.as_str()),
            self.session_cwd(spec),
        )
        .mcp_servers(Vec::new());
        let result = conn.request("session/load", to_value(&request)?).await;
        let options = result.as_ref().ok().and_then(|value| {
            serde_json::from_value::<acp::LoadSessionResponse>(value.clone())
                .ok()
                .and_then(|response| response.config_options)
        });
        let closing = {
            let mut state = self.lock_state();
            let entry = state.sessions.get_mut(&session.native_id);
            match entry {
                Some(entry) => {
                    entry.replay = Replay::Off;
                    if result.is_ok() {
                        entry.attached = Some(generation);
                    }
                    if let Some(options) = options {
                        entry.config_options = options;
                    }
                    entry.messages.close()
                }
                None => None,
            }
        };
        if let (Some(update), true) = (closing, show_history) {
            self.publish_replay(&session.native_id, generation, vec![update]);
        }
        result.map(|_| ()).map_err(rpc_failure)
    }

    fn publish_replay(&self, native: &str, generation: u64, updates: Vec<SessionUpdate>) {
        let mut state = self.lock_state();
        let instance = self.config.id.clone();
        let Some(session) = state.sessions.get_mut(native) else {
            return;
        };
        let envelopes: Vec<_> = updates
            .into_iter()
            .map(|update| {
                let mut envelope = envelope(&instance, generation, session, update);
                envelope.origin = Origin::Replay;
                envelope
            })
            .collect();
        drop(state);
        for envelope in envelopes {
            let _ = self.updates.send(envelope);
        }
    }

    fn session_cwd(&self, spec: &SessionSpec) -> PathBuf {
        let cwd = spec.cwd.clone().unwrap_or_else(|| self.cwd.clone());
        if cwd.is_absolute() {
            cwd
        } else {
            self.cwd.join(cwd)
        }
    }

    fn session_ref(&self, native_id: String) -> SessionRef {
        SessionRef {
            instance: self.config.id.clone(),
            key: SessionKey(native_id.clone()),
            native_id,
        }
    }
}

impl SessionState {
    fn new(key: SessionKey, cwd: PathBuf) -> Self {
        Self {
            key,
            cwd,
            config_options: Vec::new(),
            attached: None,
            seq: 0,
            messages: MessageTracker::default(),
            replay: Replay::Off,
            turn: None,
        }
    }
}

fn envelope(
    instance: &BackendInstanceId,
    generation: u64,
    session: &mut SessionState,
    update: SessionUpdate,
) -> UpdateEnvelope {
    session.seq += 1;
    UpdateEnvelope {
        v: PROTOCOL_VERSION,
        instance: instance.clone(),
        generation,
        session: session.key.clone(),
        turn: session.turn.as_ref().map(|turn| turn.id.clone()),
        seq: session.seq,
        origin: if session.replay == Replay::Show {
            Origin::Replay
        } else {
            Origin::Live
        },
        update,
    }
}

/// Close the open message, emit `TurnEnded`, resolve the handle and withdraw
/// pending interactions.
fn end_turn(
    instance: &BackendInstanceId,
    generation: u64,
    session: &mut SessionState,
    stop: StopReason,
    error: Option<ErrorInfo>,
    outcome: TurnOutcome,
) -> Vec<UpdateEnvelope> {
    let mut envelopes = Vec::new();
    if let Some(update) = session.messages.close() {
        envelopes.push(envelope(instance, generation, session, update));
    }
    let Some(turn) = session.turn.as_ref() else {
        return envelopes;
    };
    let turn_id = turn.id.clone();
    envelopes.push(envelope(
        instance,
        generation,
        session,
        SessionUpdate::TurnEnded {
            turn_id,
            stop,
            error,
        },
    ));
    if let Some(turn) = session.turn.take() {
        turn.cancel.cancel();
        let _ = turn.done.send(outcome);
    }
    envelopes
}

fn to_value<T: serde::Serialize>(value: &T) -> BackendResult<Value> {
    serde_json::to_value(value)
        .map_err(|e| BackendError::failed(ErrorCategory::Internal, e.to_string()))
}

fn rpc_failure(error: RpcError) -> BackendError {
    BackendError::Failed(match error {
        RpcError::Remote { code, message, .. } => remote_error(code, &message),
        RpcError::Closed => ErrorInfo::new(ErrorCategory::Agent, "the agent stopped"),
        RpcError::Invalid(why) => ErrorInfo::new(ErrorCategory::Protocol, why),
    })
}

/// Handle everything the agent sends for one generation, then settle what the
/// lost process leaves behind.
async fn dispatch(
    backend: Weak<AcpBackend>,
    generation: u64,
    conn: Arc<Connection>,
    mut incoming: mpsc::UnboundedReceiver<Incoming>,
) {
    while let Some(message) = incoming.recv().await {
        if let Incoming::Response { id, result } = message {
            // In order: every update the agent sent before this response has
            // been published, so a turn's output always precedes its end.
            conn.resolve(id, result).await;
            continue;
        }
        let Some(backend) = backend.upgrade() else {
            return;
        };
        match message {
            Incoming::Notification { method, params } => {
                if method == "session/update" {
                    on_session_update(&backend, generation, &params);
                } else {
                    tracing::debug!(%method, "ignoring notification from agent");
                }
            }
            Incoming::Request { id, method, params } => {
                let conn = Arc::clone(&conn);
                tokio::spawn(async move {
                    let result = on_request(&backend, generation, &method, params).await;
                    conn.respond(id, result);
                });
            }
            Incoming::Response { .. } => {}
        }
    }
    conn.fail_pending().await;
    if let Some(backend) = backend.upgrade() {
        backend.connection_lost(generation).await;
    }
}

fn on_session_update(backend: &AcpBackend, generation: u64, params: &Value) {
    let Some(native) = params.get("sessionId").and_then(Value::as_str) else {
        tracing::debug!("session/update without a sessionId");
        return;
    };
    let raw = params.get("update").cloned().unwrap_or(Value::Null);
    let (updates, show) = {
        let mut state = backend.lock_state();
        let Some(session) = state.sessions.get_mut(native) else {
            tracing::debug!(session = native, "update for a session Loop does not know");
            return;
        };
        if raw.get("sessionUpdate").and_then(Value::as_str) == Some("config_option_update") {
            if let Ok(update) = serde_json::from_value::<acp::ConfigOptionUpdate>(raw.clone()) {
                session.config_options = update.config_options;
            }
        }
        // A quiet reload (after a restart) replays history the surface
        // already shows; drop it.
        let show = session.replay != Replay::Hide;
        (session_update(&raw, &mut session.messages), show)
    };
    if show {
        backend.publish(native, generation, updates);
    }
}

type RequestResult = Result<Value, (i64, String)>;

async fn on_request(
    backend: &AcpBackend,
    generation: u64,
    method: &str,
    params: Value,
) -> RequestResult {
    let native = params
        .get("sessionId")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_default();
    let session = backend.session_ref(native.clone());
    let invalid = |e: serde_json::Error| (codes::INVALID_PARAMS, e.to_string());
    let failed = |e: BackendError| (codes::INTERNAL_ERROR, e.to_string());
    match method {
        "session/request_permission" => {
            let request: acp::RequestPermissionRequest =
                serde_json::from_value(params).map_err(invalid)?;
            let outcome = ask_permission(backend, generation, &native, request).await;
            let selected = match outcome {
                InteractionOutcome::Selected { native_id, .. } => {
                    json!({"outcome": "selected", "optionId": native_id})
                }
                _ => json!({"outcome": "cancelled"}),
            };
            Ok(json!({ "outcome": selected }))
        }
        "fs/read_text_file" => {
            let fs = backend
                .host
                .file_system()
                .ok_or_else(|| not_offered(method))?;
            let request: acp::ReadTextFileRequest =
                serde_json::from_value(params).map_err(invalid)?;
            let content = fs
                .read_text_file(&session, &request.path, request.line, request.limit)
                .await
                .map_err(failed)?;
            Ok(json!({ "content": content }))
        }
        "fs/write_text_file" => {
            let fs = backend
                .host
                .file_system()
                .ok_or_else(|| not_offered(method))?;
            let request: acp::WriteTextFileRequest =
                serde_json::from_value(params).map_err(invalid)?;
            fs.write_text_file(&session, &request.path, &request.content)
                .await
                .map_err(failed)?;
            Ok(json!({}))
        }
        method if method.starts_with("terminal/") => {
            let terminals = backend
                .host
                .terminals()
                .ok_or_else(|| not_offered(method))?;
            let terminal_id = || {
                params
                    .get("terminalId")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .ok_or_else(|| (codes::INVALID_PARAMS, "missing terminalId".to_owned()))
            };
            match method {
                "terminal/create" => {
                    let request: acp::CreateTerminalRequest =
                        serde_json::from_value(params.clone()).map_err(invalid)?;
                    let id = terminals
                        .create(
                            &session,
                            TerminalRequest {
                                command: request.command,
                                args: request.args,
                                env: request
                                    .env
                                    .into_iter()
                                    .map(|var| (var.name, var.value))
                                    .collect(),
                                cwd: request.cwd,
                                output_byte_limit: request.output_byte_limit,
                            },
                        )
                        .await
                        .map_err(failed)?;
                    Ok(json!({ "terminalId": id }))
                }
                "terminal/output" => {
                    let out = terminals
                        .output(&session, &terminal_id()?)
                        .await
                        .map_err(failed)?;
                    let mut value = json!({"output": out.output, "truncated": out.truncated});
                    if let Some(exit) = out.exit {
                        value["exitStatus"] =
                            json!({"exitCode": exit.exit_code, "signal": exit.signal});
                    }
                    Ok(value)
                }
                "terminal/wait_for_exit" => {
                    let exit = terminals
                        .wait_for_exit(&session, &terminal_id()?)
                        .await
                        .map_err(failed)?;
                    Ok(json!({"exitCode": exit.exit_code, "signal": exit.signal}))
                }
                "terminal/kill" => {
                    terminals
                        .kill(&session, &terminal_id()?)
                        .await
                        .map_err(failed)?;
                    Ok(json!({}))
                }
                "terminal/release" => {
                    terminals
                        .release(&session, &terminal_id()?)
                        .await
                        .map_err(failed)?;
                    Ok(json!({}))
                }
                _ => Err(not_offered(method)),
            }
        }
        _ => Err(not_offered(method)),
    }
}

fn not_offered(method: &str) -> (i64, String) {
    (
        codes::METHOD_NOT_FOUND,
        format!("Loop does not offer {method}"),
    )
}

/// Ask the user through the host, racing the turn's cancellation. Publishes
/// the request and its resolution so every surface can follow along.
async fn ask_permission(
    backend: &AcpBackend,
    generation: u64,
    native: &str,
    request: acp::RequestPermissionRequest,
) -> InteractionOutcome {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let id = InteractionId(format!(
        "acp-{}-{}",
        generation,
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let interaction = InteractionRequest::Permission {
        id: id.clone(),
        tool_call: Some(tool_call_update(request.tool_call)),
        options: permission_options(request.options),
    };
    let cancel = {
        let state = backend.lock_state();
        state
            .sessions
            .get(native)
            .and_then(|s| s.turn.as_ref())
            .map(|turn| turn.cancel.clone())
    };
    // A request outside a turn cannot be cancelled by the user; it still
    // goes to the host.
    let cancel = cancel.unwrap_or_default();
    backend.publish(
        native,
        generation,
        vec![SessionUpdate::InteractionRequested {
            request: interaction.clone(),
        }],
    );
    let outcome = tokio::select! {
        _ = cancel.cancelled() => InteractionOutcome::Cancelled,
        outcome = backend.host.interact(interaction) => outcome,
    };
    backend.publish(
        native,
        generation,
        vec![SessionUpdate::InteractionResolved {
            id,
            outcome: outcome.clone(),
        }],
    );
    outcome
}

#[async_trait]
impl AgentBackend for AcpBackend {
    fn instance(&self) -> &BackendInstanceId {
        &self.config.id
    }

    fn capabilities(&self) -> Capabilities {
        self.lock_state().capabilities
    }

    fn subscribe(&self) -> broadcast::Receiver<UpdateEnvelope> {
        self.updates.subscribe()
    }

    async fn new_session(&self, spec: SessionSpec) -> BackendResult<SessionRef> {
        let conn = self.connection().await?;
        let cwd = self.session_cwd(&spec);
        let request = acp::NewSessionRequest::new(cwd.clone()).mcp_servers(Vec::new());
        let response = conn
            .request("session/new", to_value(&request)?)
            .await
            .map_err(rpc_failure)?;
        let response: acp::NewSessionResponse = serde_json::from_value(response)
            .map_err(|e| BackendError::failed(ErrorCategory::Protocol, e.to_string()))?;
        let native_id = response.session_id.0.to_string();
        let session = self.session_ref(native_id.clone());
        let mut state = self.lock_state();
        let generation = state.generation;
        let mut entry = SessionState::new(session.key.clone(), cwd);
        entry.attached = Some(generation);
        entry.config_options = response.config_options.unwrap_or_default();
        state.sessions.insert(native_id, entry);
        Ok(session)
    }

    async fn load_session(
        &self,
        session: &SessionRef,
        spec: SessionSpec,
    ) -> BackendResult<SessionRef> {
        if !self.owns(session) {
            return Err(BackendError::SessionNotFound(session.key.clone()));
        }
        if !self.lock_state().capabilities.load_session {
            return Err(BackendError::Unsupported("reopening sessions".into()));
        }
        let conn = self.connection().await?;
        self.load(&conn, session, &spec, true).await?;
        Ok(session.clone())
    }

    async fn prompt(
        &self,
        session: &SessionRef,
        input: Vec<ContentBlock>,
    ) -> BackendResult<TurnHandle> {
        let mut prompt = Vec::with_capacity(input.len());
        for block in &input {
            prompt.push(
                acp_content(block)
                    .ok_or_else(|| BackendError::Unsupported("this prompt content".into()))?,
            );
        }
        let conn = self.attach(session).await?;
        let (done, handle) = TurnHandle::channel();
        let (turn_id, generation) = {
            let mut state = self.lock_state();
            state.next_turn += 1;
            let turn_id = TurnId(format!("turn-{}", state.next_turn));
            let generation = state.generation;
            let Some(entry) = state.sessions.get_mut(&session.native_id) else {
                return Err(BackendError::SessionNotFound(session.key.clone()));
            };
            if entry.turn.is_some() {
                return Err(BackendError::Busy);
            }
            entry.turn = Some(ActiveTurn {
                id: turn_id.clone(),
                generation,
                done,
                cancel: CancellationToken::new(),
            });
            (turn_id, generation)
        };
        self.publish(
            &session.native_id,
            generation,
            vec![SessionUpdate::TurnStarted {
                turn_id: turn_id.clone(),
            }],
        );

        let request =
            acp::PromptRequest::new(acp::SessionId::new(session.native_id.as_str()), prompt);
        let params = to_value(&request)?;
        let me = self.me.clone();
        let native = session.native_id.clone();
        tokio::spawn(async move {
            let result = conn.request("session/prompt", params).await;
            let Some(backend) = me.upgrade() else {
                return;
            };
            match result {
                Ok(value) => match serde_json::from_value::<acp::PromptResponse>(value) {
                    Ok(response) => {
                        let stop = stop_reason(response.stop_reason);
                        let outcome = if stop == StopReason::Cancelled {
                            TurnOutcome::Cancelled
                        } else {
                            TurnOutcome::Completed(stop)
                        };
                        backend.finish_turn(&native, &turn_id, stop, None, outcome);
                    }
                    Err(e) => {
                        let info = ErrorInfo::new(ErrorCategory::Protocol, e.to_string());
                        backend.finish_turn(
                            &native,
                            &turn_id,
                            StopReason::Error,
                            Some(info.clone()),
                            TurnOutcome::Failed(info),
                        );
                    }
                },
                // Connection loss settles the turn as uncertain.
                Err(RpcError::Closed) => {}
                Err(error) => {
                    let BackendError::Failed(info) = rpc_failure(error) else {
                        return;
                    };
                    let outcome = if info.category == ErrorCategory::Cancelled {
                        TurnOutcome::Cancelled
                    } else {
                        TurnOutcome::Failed(info.clone())
                    };
                    backend.finish_turn(&native, &turn_id, StopReason::Error, Some(info), outcome);
                }
            }
        });
        Ok(handle)
    }

    fn cancel(&self, session: &SessionRef) {
        if !self.owns(session) {
            return;
        }
        // Everything happens under one lock and right away, so the cancel
        // can only reach the turn that is running now, never a later one.
        let (turn_id, generation) = {
            let state = self.lock_state();
            let Some(turn) = state
                .sessions
                .get(&session.native_id)
                .and_then(|s| s.turn.as_ref())
            else {
                return;
            };
            // Pending interactions answer "cancelled", as ACP requires.
            turn.cancel.cancel();
            if let Some((g, conn)) = &state.conn {
                if *g == turn.generation {
                    let notice = acp::CancelNotification::new(acp::SessionId::new(
                        session.native_id.as_str(),
                    ));
                    if let Ok(params) = serde_json::to_value(&notice) {
                        let _ = conn.notify("session/cancel", params);
                    }
                }
            }
            (turn.id.clone(), turn.generation)
        };
        let me = self.me.clone();
        let native = session.native_id.clone();
        let timeout = self.config.cancel_timeout;
        tokio::spawn(async move {
            tokio::time::sleep(timeout).await;
            let Some(backend) = me.upgrade() else {
                return;
            };
            let still_running = backend
                .lock_state()
                .sessions
                .get(&native)
                .and_then(|s| s.turn.as_ref())
                .is_some_and(|turn| turn.id == turn_id && turn.generation == generation);
            if still_running {
                tracing::warn!(agent = %backend.config.id, "agent ignored cancel; stopping it");
                backend.kill().await;
            }
        });
    }

    fn is_busy(&self, session: &SessionRef) -> bool {
        self.owns(session)
            && self
                .lock_state()
                .sessions
                .get(&session.native_id)
                .is_some_and(|s| s.turn.is_some())
    }

    async fn shutdown(&self) {
        self.kill().await;
    }

    fn model_control(&self) -> Option<&dyn ModelControl> {
        Some(self)
    }
}

/// What a session select option sets, for lookups and error messages.
#[derive(Clone, Copy)]
enum Setting {
    Model,
    Effort,
}

impl Setting {
    fn category(self) -> acp::SessionConfigOptionCategory {
        match self {
            Self::Model => acp::SessionConfigOptionCategory::Model,
            Self::Effort => acp::SessionConfigOptionCategory::ThoughtLevel,
        }
    }

    fn unsupported(self) -> BackendError {
        BackendError::Unsupported(match self {
            Self::Model => "choosing a model".into(),
            Self::Effort => "choosing a reasoning effort".into(),
        })
    }
}

/// The session's select option for `setting`: its id and select value, when
/// the agent has one.
fn select_option(
    options: &[acp::SessionConfigOption],
    setting: Setting,
) -> Option<(String, &acp::SessionConfigSelect)> {
    options.iter().find_map(|option| {
        let matches = option.category.as_ref() == Some(&setting.category());
        match (&option.kind, matches) {
            (acp::SessionConfigKind::Select(select), true) => {
                Some((option.id.0.to_string(), select))
            }
            _ => None,
        }
    })
}

fn model_choices(select: &acp::SessionConfigSelect) -> ModelChoices {
    let choice = |option: &acp::SessionConfigSelectOption| ModelChoice {
        id: option.value.0.to_string(),
        name: option.name.clone(),
        description: option.description.clone(),
    };
    let models = match &select.options {
        acp::SessionConfigSelectOptions::Ungrouped(options) => options.iter().map(choice).collect(),
        acp::SessionConfigSelectOptions::Grouped(groups) => groups
            .iter()
            .flat_map(|group| group.options.iter().map(choice))
            .collect(),
        _ => Vec::new(),
    };
    ModelChoices {
        current: Some(select.current_value.0.to_string()),
        models,
    }
}

impl AcpBackend {
    fn choices(&self, session: &SessionRef, setting: Setting) -> BackendResult<ModelChoices> {
        if !self.owns(session) {
            return Err(BackendError::SessionNotFound(session.key.clone()));
        }
        let state = self.lock_state();
        let known = state
            .sessions
            .get(&session.native_id)
            .ok_or_else(|| BackendError::SessionNotFound(session.key.clone()))?;
        select_option(&known.config_options, setting)
            .map(|(_, select)| model_choices(select))
            .ok_or_else(|| setting.unsupported())
    }

    async fn choose(
        &self,
        session: &SessionRef,
        setting: Setting,
        value: &str,
    ) -> BackendResult<()> {
        if !self.owns(session) {
            return Err(BackendError::SessionNotFound(session.key.clone()));
        }
        let config_id = {
            let state = self.lock_state();
            let known = state
                .sessions
                .get(&session.native_id)
                .ok_or_else(|| BackendError::SessionNotFound(session.key.clone()))?;
            let (config_id, select) = select_option(&known.config_options, setting)
                .ok_or_else(|| setting.unsupported())?;
            if !model_choices(select).models.iter().any(|m| m.id == value) {
                return Err(BackendError::Unsupported(format!("`{value}`")));
            }
            config_id
        };
        let conn = self.attach(session).await?;
        let response = conn
            .request(
                "session/set_config_option",
                json!({"sessionId": session.native_id, "configId": config_id, "value": value}),
            )
            .await
            .map_err(rpc_failure)?;
        if let Ok(response) =
            serde_json::from_value::<acp::SetSessionConfigOptionResponse>(response)
        {
            if let Some(known) = self.lock_state().sessions.get_mut(&session.native_id) {
                known.config_options = response.config_options;
            }
        }
        Ok(())
    }
}

#[async_trait]
impl ModelControl for AcpBackend {
    async fn models(&self, session: &SessionRef) -> BackendResult<ModelChoices> {
        self.choices(session, Setting::Model)
    }

    async fn set_model(&self, session: &SessionRef, model: &str) -> BackendResult<()> {
        self.choose(session, Setting::Model, model).await
    }

    async fn efforts(&self, session: &SessionRef) -> BackendResult<ModelChoices> {
        self.choices(session, Setting::Effort)
    }

    async fn set_effort(&self, session: &SessionRef, effort: &str) -> BackendResult<()> {
        self.choose(session, Setting::Effort, effort).await
    }
}
