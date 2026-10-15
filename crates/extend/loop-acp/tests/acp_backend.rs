//! AcpBackend against a scripted fake agent that speaks raw ACP JSON-RPC
//! (written by hand, not with an SDK, so it pins the wire format).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use loop_acp::{AcpBackend, AgentConfig, AgentLink, Connector};
use loop_protocol::{
    AgentBackend, BackendError, BackendResult, ContentBlock, ErrorCategory, HostFileSystem,
    HostServices, InteractionOutcome, InteractionRequest, Origin, PermissionKind, Role, SessionRef,
    SessionSpec, SessionUpdate, StopReason, ToolContent, TurnOutcome, UpdateEnvelope,
};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{broadcast, mpsc, oneshot, Notify};
use tokio::task::JoinSet;

// ---------------------------------------------------------------- fake agent

/// What the fake agent saw, shared with the test.
#[derive(Default)]
struct World {
    load_session: bool,
    connections: AtomicU64,
    sessions: AtomicU64,
    /// Every message the agent received.
    received: Mutex<Vec<Value>>,
    /// Answers to the agent's own requests, by method.
    answers: Mutex<Vec<(String, Value)>>,
    cancel: Notify,
}

impl World {
    fn answer_to(&self, method: &str) -> Option<Value> {
        self.answers
            .lock()
            .unwrap()
            .iter()
            .find(|(m, _)| m == method)
            .map(|(_, v)| v.clone())
    }
}

struct FakeConnector(Arc<World>);

#[async_trait]
impl Connector for FakeConnector {
    async fn connect(&self, _config: &AgentConfig, _cwd: &Path) -> std::io::Result<AgentLink> {
        self.0.connections.fetch_add(1, Ordering::SeqCst);
        let (loop_end, agent_end) = tokio::io::duplex(1 << 16);
        let (agent_read, agent_write) = tokio::io::split(agent_end);
        tokio::spawn(fake_agent(agent_read, agent_write, Arc::clone(&self.0)));
        let (read, write) = tokio::io::split(loop_end);
        Ok(AgentLink {
            reader: Box::new(read),
            writer: Box::new(write),
            process: None,
        })
    }
}

/// Handle to the agent's outgoing side.
#[derive(Clone)]
struct Agent {
    out: mpsc::UnboundedSender<Value>,
    pending: Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>>,
    next: Arc<AtomicU64>,
    crash: Arc<Notify>,
}

impl Agent {
    fn update(&self, session: &str, update: Value) {
        let _ = self
            .out
            .send(json!({"jsonrpc": "2.0", "method": "session/update",
            "params": {"sessionId": session, "update": update}}));
    }

    fn text(&self, session: &str, text: &str) {
        self.update(
            session,
            json!({"sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": text}}),
        );
    }

    async fn request(&self, method: &str, params: Value) -> Value {
        let id = self.next.fetch_add(1, Ordering::SeqCst) as i64 + 1000;
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let _ = self
            .out
            .send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        rx.await.unwrap_or(Value::Null)
    }
}

async fn fake_agent(
    read: tokio::io::ReadHalf<tokio::io::DuplexStream>,
    mut write: tokio::io::WriteHalf<tokio::io::DuplexStream>,
    world: Arc<World>,
) {
    let (out, mut to_write) = mpsc::unbounded_channel::<Value>();
    let writer = tokio::spawn(async move {
        while let Some(message) = to_write.recv().await {
            if write
                .write_all(format!("{message}\n").as_bytes())
                .await
                .is_err()
            {
                break;
            }
        }
    });
    let agent = Agent {
        out,
        pending: Arc::default(),
        next: Arc::default(),
        crash: Arc::new(Notify::new()),
    };
    let mut handlers = JoinSet::new();
    let mut lines = BufReader::new(read).lines();
    loop {
        let line = tokio::select! {
            _ = agent.crash.notified() => break,
            line = lines.next_line() => match line {
                Ok(Some(line)) => line,
                _ => break,
            },
        };
        let message: Value = serde_json::from_str(&line).unwrap();
        world.received.lock().unwrap().push(message.clone());
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .map(str::to_owned);
        match (method, message.get("id").cloned()) {
            (Some(method), Some(id)) => {
                let (agent, world) = (agent.clone(), Arc::clone(&world));
                let params = message["params"].clone();
                handlers.spawn(async move {
                    let reply = handle(&agent, &world, &method, params).await;
                    let mut response = json!({"jsonrpc": "2.0", "id": id});
                    match reply {
                        Ok(result) => response["result"] = result,
                        Err(error) => response["error"] = error,
                    }
                    let _ = agent.out.send(response);
                });
            }
            (Some(method), None) => {
                if method == "session/cancel" {
                    world.cancel.notify_one();
                }
            }
            (None, Some(id)) => {
                let waiter = id
                    .as_i64()
                    .and_then(|id| agent.pending.lock().unwrap().remove(&id));
                if let Some(waiter) = waiter {
                    let _ = waiter.send(message);
                }
            }
            (None, None) => {}
        }
    }
    // Crash or end of input: stop everything; dropping the writer closes
    // Loop's side.
    handlers.abort_all();
    drop(agent);
    writer.abort();
}

async fn handle(agent: &Agent, world: &World, method: &str, params: Value) -> Result<Value, Value> {
    match method {
        "initialize" => Ok(json!({
            "protocolVersion": 1,
            "agentCapabilities": {"loadSession": world.load_session, "promptCapabilities": {"image": true}},
            "authMethods": [{"id": "chatgpt", "name": "Sign in with ChatGPT"}]
        })),
        "authenticate" => Ok(json!({})),
        "session/new" => {
            let n = world.sessions.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(
                json!({"sessionId": format!("s{n}"), "configOptions": config_options("gpt-a", "low")}),
            )
        }
        "session/set_config_option" => {
            let value = params["value"].as_str().unwrap_or_default();
            let options = match params["configId"].as_str() {
                Some("effort") => config_options("gpt-a", value),
                _ => config_options(value, "low"),
            };
            Ok(json!({"configOptions": options}))
        }
        "session/load" => {
            let session = params["sessionId"].as_str().unwrap_or_default().to_owned();
            agent.update(
                &session,
                json!({"sessionUpdate": "user_message_chunk",
                "content": {"type": "text", "text": "earlier question"}}),
            );
            agent.text(&session, "earlier answer");
            Ok(json!({}))
        }
        "session/prompt" => {
            let session = params["sessionId"].as_str().unwrap_or_default().to_owned();
            let text = params["prompt"][0]["text"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            prompt(agent, world, &session, &text).await
        }
        _ => Err(json!({"code": -32601, "message": "unknown"})),
    }
}

fn config_options(model: &str, effort: &str) -> Value {
    json!([
        {"id": "model", "name": "Model", "category": "model", "type": "select",
        "currentValue": model,
        "options": [{"value": "gpt-a", "name": "GPT A"}, {"value": "gpt-b", "name": "GPT B", "description": "faster"}]},
        {"id": "effort", "name": "Reasoning", "category": "thought_level", "type": "select",
        "currentValue": effort,
        "options": [{"value": "low", "name": "Low"}, {"value": "high", "name": "High"}]}
    ])
}

async fn prompt(agent: &Agent, world: &World, session: &str, text: &str) -> Result<Value, Value> {
    let end_turn = Ok(json!({"stopReason": "end_turn"}));
    match text {
        "hello" => {
            agent.text(session, "Hi ");
            agent.text(session, "there");
            end_turn
        }
        "edit" => {
            agent.update(
                session,
                json!({"sessionUpdate": "tool_call", "toolCallId": "t1",
                "title": "Edit a.txt", "kind": "edit", "status": "pending",
                "content": [{"type": "diff", "path": "/w/a.txt", "oldText": "a", "newText": "b"}]}),
            );
            let answer = agent
                .request("session/request_permission", json!({"sessionId": session,
                    "toolCall": {"toolCallId": "t1", "title": "Edit a.txt"},
                    "options": [
                        {"optionId": "approved", "name": "Yes", "kind": "allow_once"},
                        {"optionId": "approved-for-session", "name": "Always", "kind": "allow_always"},
                        {"optionId": "abort", "name": "No", "kind": "reject_once"}]}))
                .await;
            world.answers.lock().unwrap().push((
                "session/request_permission".into(),
                answer["result"].clone(),
            ));
            agent.update(
                session,
                json!({"sessionUpdate": "tool_call_update", "toolCallId": "t1",
                "status": "completed"}),
            );
            agent.text(session, "done");
            end_turn
        }
        "wait" => {
            let answer = agent
                .request(
                    "session/request_permission",
                    json!({"sessionId": session,
                    "toolCall": {"toolCallId": "t2", "title": "Run rm"},
                    "options": [{"optionId": "ok", "name": "Yes", "kind": "allow_once"}]}),
                )
                .await;
            world.answers.lock().unwrap().push((
                "session/request_permission".into(),
                answer["result"].clone(),
            ));
            world.cancel.notified().await;
            Ok(json!({"stopReason": "cancelled"}))
        }
        "burst" => {
            for n in 0..40 {
                agent.text(session, &format!("{n},"));
            }
            end_turn
        }
        "hang" => std::future::pending().await,
        "crash" => {
            agent.text(session, "about to crash");
            tokio::time::sleep(Duration::from_millis(50)).await;
            agent.crash.notify_one();
            std::future::pending().await
        }
        "fail" => Err(json!({"code": -32000, "message": "Authentication required"})),
        "fs" => {
            let read = agent
                .request(
                    "fs/read_text_file",
                    json!({"sessionId": session, "path": "/w/a.txt"}),
                )
                .await;
            let content = read["result"]["content"]
                .as_str()
                .unwrap_or("<none>")
                .to_owned();
            world
                .answers
                .lock()
                .unwrap()
                .push(("fs/read_text_file".into(), read.clone()));
            let wrote = agent
                .request(
                    "fs/write_text_file",
                    json!({"sessionId": session, "path": "/w/a.txt",
                    "content": format!("{content}!")}),
                )
                .await;
            world
                .answers
                .lock()
                .unwrap()
                .push(("fs/write_text_file".into(), wrote));
            agent.text(session, &content);
            end_turn
        }
        "mystery" => {
            agent.update(session, json!({"sessionUpdate": "hologram", "beam": 1}));
            let answer = agent
                .request("x/teleport", json!({"sessionId": session}))
                .await;
            world
                .answers
                .lock()
                .unwrap()
                .push(("x/teleport".into(), answer));
            agent.text(session, "survived");
            end_turn
        }
        _ => end_turn,
    }
}

// ------------------------------------------------------------------ fake host

#[derive(Default)]
struct FakeHost {
    /// Answer every permission request with this option id; `None` waits forever.
    answer: Option<String>,
    files: Option<Mutex<HashMap<PathBuf, String>>>,
    asked: Mutex<Vec<InteractionRequest>>,
}

#[async_trait]
impl HostServices for FakeHost {
    async fn interact(&self, request: InteractionRequest) -> InteractionOutcome {
        self.asked.lock().unwrap().push(request);
        match &self.answer {
            Some(id) => InteractionOutcome::Selected {
                native_id: id.clone(),
                note: None,
            },
            None => std::future::pending().await,
        }
    }

    fn file_system(&self) -> Option<&dyn HostFileSystem> {
        self.files.as_ref().map(|_| self as &dyn HostFileSystem)
    }
}

#[async_trait]
impl HostFileSystem for FakeHost {
    async fn read_text_file(
        &self,
        _session: &SessionRef,
        path: &Path,
        _line: Option<u32>,
        _limit: Option<u32>,
    ) -> BackendResult<String> {
        let files = self.files.as_ref().unwrap().lock().unwrap();
        Ok(files.get(path).cloned().unwrap_or_default())
    }

    async fn write_text_file(
        &self,
        _session: &SessionRef,
        path: &Path,
        content: &str,
    ) -> BackendResult<()> {
        let mut files = self.files.as_ref().unwrap().lock().unwrap();
        files.insert(path.to_path_buf(), content.to_owned());
        Ok(())
    }
}

// ------------------------------------------------------------------- helpers

struct Fixture {
    backend: Arc<AcpBackend>,
    world: Arc<World>,
    host: Arc<FakeHost>,
}

async fn fixture(world: World, host: FakeHost) -> Fixture {
    let world = Arc::new(world);
    let host = Arc::new(host);
    let mut config = AgentConfig::new("fake", "fake-agent");
    config.cancel_timeout = Duration::from_millis(200);
    let backend = AcpBackend::start(
        config,
        PathBuf::from("/w"),
        Arc::new(FakeConnector(Arc::clone(&world))),
        Arc::clone(&host) as Arc<dyn HostServices>,
    )
    .await
    .unwrap();
    Fixture {
        backend,
        world,
        host,
    }
}

async fn until_turn_end(
    rx: &mut broadcast::Receiver<UpdateEnvelope>,
    session: &SessionRef,
) -> Vec<UpdateEnvelope> {
    let mut out = Vec::new();
    loop {
        let envelope = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("turn ends")
            .unwrap();
        if envelope.session != session.key {
            continue;
        }
        let end = matches!(envelope.update, SessionUpdate::TurnEnded { .. });
        out.push(envelope);
        if end {
            return out;
        }
    }
}

fn agent_text(envelopes: &[UpdateEnvelope]) -> String {
    envelopes
        .iter()
        .filter_map(|e| match &e.update {
            SessionUpdate::MessageChunk {
                role: Role::Agent,
                content: ContentBlock::Text { text },
                ..
            } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn say(text: &str) -> Vec<ContentBlock> {
    vec![ContentBlock::text(text)]
}

async fn outcome(handle: loop_protocol::TurnHandle) -> TurnOutcome {
    tokio::time::timeout(Duration::from_secs(5), handle.outcome())
        .await
        .expect("turn resolves")
}

// --------------------------------------------------------------------- tests

#[tokio::test]
async fn text_turn_streams_with_boundaries_and_one_outcome() {
    let fx = fixture(World::default(), FakeHost::default()).await;
    let session = fx
        .backend
        .new_session(SessionSpec::default())
        .await
        .unwrap();
    let mut rx = fx.backend.subscribe();
    let handle = fx.backend.prompt(&session, say("hello")).await.unwrap();
    let envelopes = until_turn_end(&mut rx, &session).await;
    assert_eq!(
        outcome(handle).await,
        TurnOutcome::Completed(StopReason::EndTurn)
    );

    assert_eq!(agent_text(&envelopes), "Hi there");
    let kinds: Vec<_> = envelopes
        .iter()
        .map(|e| {
            serde_json::to_value(&e.update).unwrap()["type"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(
        kinds,
        [
            "turn_started",
            "message_started",
            "message_chunk",
            "message_chunk",
            "message_ended",
            "turn_ended"
        ]
    );
    let mut seq = 0;
    for envelope in &envelopes {
        assert_eq!(envelope.generation, 1);
        assert_eq!(envelope.origin, Origin::Live);
        assert!(envelope.seq > seq);
        seq = envelope.seq;
        assert!(envelope.turn.is_some());
    }
    // Initialize advertised no file system when the host has none.
    let init = fx.world.received.lock().unwrap()[0].clone();
    assert_eq!(init["method"], "initialize");
    assert_eq!(
        init["params"]["clientCapabilities"]["fs"]["readTextFile"],
        false
    );
}

#[tokio::test]
async fn permission_uses_native_option_ids() {
    let host = FakeHost {
        answer: Some("approved-for-session".into()),
        ..FakeHost::default()
    };
    let fx = fixture(World::default(), host).await;
    let session = fx
        .backend
        .new_session(SessionSpec::default())
        .await
        .unwrap();
    let mut rx = fx.backend.subscribe();
    let handle = fx.backend.prompt(&session, say("edit")).await.unwrap();
    let envelopes = until_turn_end(&mut rx, &session).await;
    assert_eq!(
        outcome(handle).await,
        TurnOutcome::Completed(StopReason::EndTurn)
    );

    assert_eq!(
        fx.world.answer_to("session/request_permission").unwrap(),
        json!({"outcome": {"outcome": "selected", "optionId": "approved-for-session"}})
    );
    let InteractionRequest::Permission {
        options, tool_call, ..
    } = &fx.host.asked.lock().unwrap()[0]
    else {
        panic!("permission expected");
    };
    assert_eq!(options[1].native_id, "approved-for-session");
    assert_eq!(options[1].kind_hint, PermissionKind::AllowAlways);
    assert_eq!(tool_call.as_ref().unwrap().id.as_str(), "t1");
    assert!(envelopes.iter().any(|e| matches!(&e.update,
        SessionUpdate::ToolCall(call) if matches!(&call.content[0], ToolContent::Diff { new_text, .. } if new_text == "b"))));
    assert!(envelopes
        .iter()
        .any(|e| matches!(e.update, SessionUpdate::InteractionResolved { .. })));
}

#[tokio::test]
async fn cancel_answers_pending_permission_and_ends_cancelled() {
    let fx = fixture(World::default(), FakeHost::default()).await;
    let session = fx
        .backend
        .new_session(SessionSpec::default())
        .await
        .unwrap();
    let mut rx = fx.backend.subscribe();
    let handle = fx.backend.prompt(&session, say("wait")).await.unwrap();
    loop {
        let envelope = rx.recv().await.unwrap();
        if matches!(envelope.update, SessionUpdate::InteractionRequested { .. }) {
            break;
        }
    }
    assert!(fx.backend.is_busy(&session));
    fx.backend.cancel(&session);
    assert_eq!(outcome(handle).await, TurnOutcome::Cancelled);
    assert_eq!(
        fx.world.answer_to("session/request_permission").unwrap(),
        json!({"outcome": {"outcome": "cancelled"}})
    );
    assert!(!fx.backend.is_busy(&session));
}

#[tokio::test]
async fn ignored_cancel_kills_the_agent_and_the_next_prompt_restarts_it() {
    let world = World {
        load_session: true,
        ..World::default()
    };
    let fx = fixture(world, FakeHost::default()).await;
    let session = fx
        .backend
        .new_session(SessionSpec::default())
        .await
        .unwrap();
    let mut rx = fx.backend.subscribe();
    let handle = fx.backend.prompt(&session, say("hang")).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    fx.backend.cancel(&session);
    assert_eq!(outcome(handle).await, TurnOutcome::Uncertain);
    let ended = until_turn_end(&mut rx, &session).await;
    assert!(matches!(
        &ended.last().unwrap().update,
        SessionUpdate::TurnEnded { stop: StopReason::Error, error: Some(e), .. } if e.category == ErrorCategory::Agent
    ));

    // The next prompt starts a new process, quietly reopens the session and works.
    let handle = fx.backend.prompt(&session, say("hello")).await.unwrap();
    let envelopes = until_turn_end(&mut rx, &session).await;
    assert_eq!(
        outcome(handle).await,
        TurnOutcome::Completed(StopReason::EndTurn)
    );
    assert_eq!(fx.world.connections.load(Ordering::SeqCst), 2);
    assert!(envelopes
        .iter()
        .all(|e| e.generation == 2 && e.origin == Origin::Live));
    assert_eq!(
        agent_text(&envelopes),
        "Hi there",
        "reload history stays hidden"
    );
}

#[tokio::test]
async fn crash_ends_every_live_turn_in_the_process_as_uncertain() {
    let fx = fixture(World::default(), FakeHost::default()).await;
    let first = fx
        .backend
        .new_session(SessionSpec::default())
        .await
        .unwrap();
    let second = fx
        .backend
        .new_session(SessionSpec::default())
        .await
        .unwrap();
    let waiting = fx.backend.prompt(&first, say("hang")).await.unwrap();
    let crashing = fx.backend.prompt(&second, say("crash")).await.unwrap();
    assert_eq!(outcome(crashing).await, TurnOutcome::Uncertain);
    assert_eq!(outcome(waiting).await, TurnOutcome::Uncertain);
    // Without load_session support, the lost sessions cannot be reopened.
    let err = fx.backend.prompt(&first, say("hello")).await.unwrap_err();
    assert!(err.to_string().contains("start a new one"), "{err}");
}

#[tokio::test]
async fn auth_error_is_categorized() {
    let fx = fixture(World::default(), FakeHost::default()).await;
    let session = fx
        .backend
        .new_session(SessionSpec::default())
        .await
        .unwrap();
    let handle = fx.backend.prompt(&session, say("fail")).await.unwrap();
    match outcome(handle).await {
        TurnOutcome::Failed(info) => assert_eq!(info.category, ErrorCategory::Auth),
        other => panic!("{other:?}"),
    }
    assert_eq!(fx.backend.auth_methods().len(), 1);
}

#[tokio::test]
async fn file_requests_go_to_the_host() {
    let host = FakeHost {
        files: Some(Mutex::new(HashMap::from([(
            PathBuf::from("/w/a.txt"),
            "hello".to_owned(),
        )]))),
        ..FakeHost::default()
    };
    let fx = fixture(World::default(), host).await;
    let session = fx
        .backend
        .new_session(SessionSpec::default())
        .await
        .unwrap();
    let mut rx = fx.backend.subscribe();
    let handle = fx.backend.prompt(&session, say("fs")).await.unwrap();
    let envelopes = until_turn_end(&mut rx, &session).await;
    outcome(handle).await;
    assert_eq!(agent_text(&envelopes), "hello");
    let files = fx.host.files.as_ref().unwrap().lock().unwrap();
    assert_eq!(files[Path::new("/w/a.txt")], "hello!");
    let init = fx.world.received.lock().unwrap()[0].clone();
    assert_eq!(
        init["params"]["clientCapabilities"]["fs"]["writeTextFile"],
        true
    );
}

#[tokio::test]
async fn file_requests_without_host_support_get_an_error() {
    let fx = fixture(World::default(), FakeHost::default()).await;
    let session = fx
        .backend
        .new_session(SessionSpec::default())
        .await
        .unwrap();
    let handle = fx.backend.prompt(&session, say("fs")).await.unwrap();
    outcome(handle).await;
    let read = fx.world.answer_to("fs/read_text_file").unwrap();
    assert_eq!(read["error"]["code"], -32601);
}

#[tokio::test]
async fn unknown_requests_get_an_error_and_unknown_updates_are_kept() {
    let fx = fixture(World::default(), FakeHost::default()).await;
    let session = fx
        .backend
        .new_session(SessionSpec::default())
        .await
        .unwrap();
    let mut rx = fx.backend.subscribe();
    let handle = fx.backend.prompt(&session, say("mystery")).await.unwrap();
    let envelopes = until_turn_end(&mut rx, &session).await;
    assert_eq!(
        outcome(handle).await,
        TurnOutcome::Completed(StopReason::EndTurn)
    );
    assert_eq!(
        fx.world.answer_to("x/teleport").unwrap()["error"]["code"],
        -32601
    );
    assert!(envelopes.iter().any(|e| matches!(&e.update,
        SessionUpdate::Unknown(raw) if raw["sessionUpdate"] == "hologram")));
    assert_eq!(agent_text(&envelopes), "survived");
}

#[tokio::test]
async fn load_session_replays_history_as_replay() {
    let world = World {
        load_session: true,
        ..World::default()
    };
    let fx = fixture(world, FakeHost::default()).await;
    let session = fx
        .backend
        .new_session(SessionSpec::default())
        .await
        .unwrap();
    let mut rx = fx.backend.subscribe();
    fx.backend
        .load_session(&session, SessionSpec::default())
        .await
        .unwrap();
    let mut replayed = Vec::new();
    while let Ok(Ok(envelope)) = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
        replayed.push(envelope);
    }
    assert!(!replayed.is_empty());
    assert!(replayed.iter().all(|e| e.origin == Origin::Replay));
    assert_eq!(agent_text(&replayed), "earlier answer");
    assert!(replayed.iter().any(|e| matches!(
        &e.update,
        SessionUpdate::MessageChunk {
            role: Role::User,
            ..
        }
    )));
}

#[tokio::test]
async fn second_prompt_while_busy_is_rejected() {
    let fx = fixture(World::default(), FakeHost::default()).await;
    let session = fx
        .backend
        .new_session(SessionSpec::default())
        .await
        .unwrap();
    let _running = fx.backend.prompt(&session, say("hang")).await.unwrap();
    assert_eq!(
        fx.backend.prompt(&session, say("hello")).await.unwrap_err(),
        loop_protocol::BackendError::Busy
    );
}

#[tokio::test]
async fn output_always_arrives_before_the_turn_ends() {
    let fx = fixture(World::default(), FakeHost::default()).await;
    let session = fx
        .backend
        .new_session(SessionSpec::default())
        .await
        .unwrap();
    for _ in 0..5 {
        let mut rx = fx.backend.subscribe();
        let handle = fx.backend.prompt(&session, say("burst")).await.unwrap();
        let envelopes = until_turn_end(&mut rx, &session).await;
        outcome(handle).await;
        let expected: String = (0..40).map(|n| format!("{n},")).collect();
        assert_eq!(agent_text(&envelopes), expected);
    }
}

#[tokio::test]
async fn the_prompt_after_a_cancelled_turn_works() {
    let fx = fixture(World::default(), FakeHost::default()).await;
    let session = fx
        .backend
        .new_session(SessionSpec::default())
        .await
        .unwrap();
    let mut rx = fx.backend.subscribe();
    let handle = fx.backend.prompt(&session, say("wait")).await.unwrap();
    loop {
        let envelope = rx.recv().await.unwrap();
        if matches!(envelope.update, SessionUpdate::InteractionRequested { .. }) {
            break;
        }
    }
    fx.backend.cancel(&session);
    assert_eq!(outcome(handle).await, TurnOutcome::Cancelled);
    until_turn_end(&mut rx, &session).await;

    let handle = fx.backend.prompt(&session, say("hello")).await.unwrap();
    let envelopes = until_turn_end(&mut rx, &session).await;
    assert_eq!(
        outcome(handle).await,
        TurnOutcome::Completed(StopReason::EndTurn)
    );
    assert_eq!(agent_text(&envelopes), "Hi there");
    let cancels = fx
        .world
        .received
        .lock()
        .unwrap()
        .iter()
        .filter(|m| m["method"] == "session/cancel")
        .count();
    assert_eq!(cancels, 1, "the cancel reached only the cancelled turn");
}

#[tokio::test]
async fn models_come_from_the_session_config_and_can_be_switched() {
    let fx = fixture(World::default(), FakeHost::default()).await;
    let session = fx
        .backend
        .new_session(SessionSpec::default())
        .await
        .unwrap();
    let control = fx.backend.model_control().unwrap();

    let choices = control.models(&session).await.unwrap();
    assert_eq!(choices.current.as_deref(), Some("gpt-a"));
    let ids: Vec<_> = choices.models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["gpt-a", "gpt-b"]);
    assert_eq!(choices.models[1].description.as_deref(), Some("faster"));

    control.set_model(&session, "gpt-b").await.unwrap();
    assert_eq!(
        control.models(&session).await.unwrap().current.as_deref(),
        Some("gpt-b")
    );
    let sent = fx
        .world
        .received
        .lock()
        .unwrap()
        .iter()
        .find(|m| m["method"] == "session/set_config_option")
        .cloned()
        .unwrap();
    assert_eq!(sent["params"]["configId"], "model");
    assert_eq!(sent["params"]["value"], "gpt-b");

    assert!(control.set_model(&session, "gpt-z").await.is_err());
}

#[tokio::test]
async fn reasoning_effort_comes_from_the_session_config_and_can_be_switched() {
    let fx = fixture(World::default(), FakeHost::default()).await;
    let session = fx
        .backend
        .new_session(SessionSpec::default())
        .await
        .unwrap();
    let control = fx.backend.model_control().unwrap();

    let efforts = control.efforts(&session).await.unwrap();
    assert_eq!(efforts.current.as_deref(), Some("low"));
    let ids: Vec<_> = efforts.models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["low", "high"]);

    control.set_effort(&session, "high").await.unwrap();
    assert_eq!(
        control.efforts(&session).await.unwrap().current.as_deref(),
        Some("high")
    );
    let sent = fx
        .world
        .received
        .lock()
        .unwrap()
        .iter()
        .find(|m| m["method"] == "session/set_config_option")
        .cloned()
        .unwrap();
    assert_eq!(sent["params"]["configId"], "effort");
    assert_eq!(sent["params"]["value"], "high");

    assert!(control.set_effort(&session, "ultra").await.is_err());
}

#[tokio::test]
async fn sessions_of_another_backend_are_refused() {
    let fx = fixture(World::default(), FakeHost::default()).await;
    let ours = fx
        .backend
        .new_session(SessionSpec::default())
        .await
        .unwrap();
    // Same native id, but another agent's instance.
    let theirs = SessionRef {
        instance: loop_protocol::BackendInstanceId::from("other-agent"),
        ..ours.clone()
    };
    let control = fx.backend.model_control().unwrap();

    assert!(matches!(
        fx.backend
            .prompt(&theirs, vec![ContentBlock::text("hello")])
            .await,
        Err(BackendError::SessionNotFound(_))
    ));
    assert!(matches!(
        control.models(&theirs).await,
        Err(BackendError::SessionNotFound(_))
    ));
    assert!(matches!(
        control.set_effort(&theirs, "high").await,
        Err(BackendError::SessionNotFound(_))
    ));
    fx.backend.cancel(&theirs);
    assert!(!fx.backend.is_busy(&theirs));
    let methods: Vec<_> = fx
        .world
        .received
        .lock()
        .unwrap()
        .iter()
        .filter_map(|m| m["method"].as_str().map(str::to_owned))
        .collect();
    assert!(
        !methods.iter().any(|m| m == "session/prompt"
            || m == "session/cancel"
            || m == "session/set_config_option"),
        "nothing reached the agent: {methods:?}"
    );
    // Our own session still works.
    assert!(control.models(&ours).await.is_ok());
}
