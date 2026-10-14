//! AgentManager with a fake ACP agent: sessions are recorded for `--resume`,
//! resuming replays history, and both kinds of sign-in work.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use loop_acp::{AgentConfig, AgentLink, Connector};
use loop_app_core::agents::{find_recorded_session, index_path, AgentEntry, AgentManager};
use loop_protocol::{
    AgentBackend, ContentBlock, HostServices, InteractionOutcome, InteractionRequest, Origin, Role,
    SessionUpdate, StopReason, TurnOutcome,
};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// A fake agent: answers the ACP calls Loop makes and records what it saw.
struct FakeAgent {
    seen: Arc<Mutex<Vec<Value>>>,
}

#[async_trait]
impl Connector for FakeAgent {
    async fn connect(&self, config: &AgentConfig, _cwd: &Path) -> std::io::Result<AgentLink> {
        assert_eq!(
            config.args,
            ["acp"],
            "the registry entry's args reach the connector"
        );
        let (loop_end, agent_end) = tokio::io::duplex(1 << 16);
        let (read, mut write) = tokio::io::split(agent_end);
        let seen = Arc::clone(&self.seen);
        tokio::spawn(async move {
            let mut lines = BufReader::new(read).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let message: Value = serde_json::from_str(&line).unwrap();
                seen.lock().unwrap().push(message.clone());
                let Some(id) = message.get("id").cloned() else {
                    continue;
                };
                let session = message["params"]["sessionId"]
                    .as_str()
                    .unwrap_or("")
                    .to_owned();
                let mut out = Vec::new();
                let result = match message["method"].as_str().unwrap_or("") {
                    "initialize" => json!({"protocolVersion": 1,
                    "agentCapabilities": {"loadSession": true},
                    "authMethods": [
                        {"id": "chatgpt", "name": "Sign in with ChatGPT"},
                        {"type": "terminal", "id": "setup", "name": "Set up", "args": ["--setup"]}
                    ]}),
                    "authenticate" => json!({}),
                    "session/new" => json!({"sessionId": "agent-session-1"}),
                    "session/load" => {
                        out.push(
                            json!({"jsonrpc": "2.0", "method": "session/update", "params": {
                            "sessionId": session, "update": {"sessionUpdate": "agent_message_chunk",
                            "content": {"type": "text", "text": "from before"}}}}),
                        );
                        json!({})
                    }
                    "session/prompt" => {
                        out.push(
                            json!({"jsonrpc": "2.0", "method": "session/update", "params": {
                            "sessionId": session, "update": {"sessionUpdate": "agent_message_chunk",
                            "content": {"type": "text", "text": "ok"}}}}),
                        );
                        json!({"stopReason": "end_turn"})
                    }
                    _ => json!(null),
                };
                out.push(json!({"jsonrpc": "2.0", "id": id, "result": result}));
                for message in out {
                    if write
                        .write_all(format!("{message}\n").as_bytes())
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
            }
        });
        let (read, write) = tokio::io::split(loop_end);
        Ok(AgentLink {
            reader: Box::new(read),
            writer: Box::new(write),
            process: None,
        })
    }
}

/// Approves everything and counts requests.
#[derive(Default)]
struct CountingHost(Mutex<usize>);

#[async_trait]
impl HostServices for CountingHost {
    async fn interact(&self, _request: InteractionRequest) -> InteractionOutcome {
        *self.0.lock().unwrap() += 1;
        InteractionOutcome::Cancelled
    }
}

fn entry() -> AgentEntry {
    serde_json::from_value(
        json!({"id": "fake", "name": "Fake", "command": "fake-agent", "args": ["acp"]}),
    )
    .unwrap()
}

fn manager(dir: &Path, seen: &Arc<Mutex<Vec<Value>>>) -> AgentManager {
    AgentManager::with_connector(
        vec![entry()],
        dir.to_path_buf(),
        index_path(dir),
        Arc::new(FakeAgent {
            seen: Arc::clone(seen),
        }),
    )
}

#[tokio::test]
async fn new_session_is_recorded_and_prompts_work() {
    let dir = tempfile::tempdir().unwrap();
    let seen = Arc::default();
    let agents = manager(dir.path(), &seen);
    agents.set_host(Arc::new(CountingHost::default()));

    let (backend, session) = agents.new_session("fake").await.unwrap();
    assert_eq!(session.native_id, "agent-session-1");
    let record = find_recorded_session(dir.path(), "agent-session-1").unwrap();
    assert_eq!(record.agent, "fake");
    assert_eq!(record.cwd, dir.path());

    let mut updates = backend.subscribe();
    let turn = backend
        .prompt(&session, vec![ContentBlock::text("hi")])
        .await
        .unwrap();
    assert_eq!(
        turn.outcome().await,
        TurnOutcome::Completed(StopReason::EndTurn)
    );
    let mut text = String::new();
    while let Ok(Ok(envelope)) =
        tokio::time::timeout(Duration::from_millis(100), updates.recv()).await
    {
        if let SessionUpdate::MessageChunk {
            role: Role::Agent,
            content,
            ..
        } = envelope.update
        {
            text.push_str(content.as_text().unwrap_or_default());
        }
    }
    assert_eq!(text, "ok");
    agents.shutdown().await;
}

#[tokio::test]
async fn resume_reopens_the_recorded_session_with_replay() {
    let dir = tempfile::tempdir().unwrap();
    let seen: Arc<Mutex<Vec<Value>>> = Arc::default();
    let agents = manager(dir.path(), &seen);
    agents.new_session("fake").await.unwrap();
    let record = agents.find_session("agent-session-1").unwrap();

    let backend = agents.backend("fake").await.unwrap();
    let mut updates = backend.subscribe();
    let (_, session) = agents.resume(&record).await.unwrap();
    assert_eq!(session.native_id, "agent-session-1");
    let mut replayed = Vec::new();
    while let Ok(Ok(envelope)) =
        tokio::time::timeout(Duration::from_millis(100), updates.recv()).await
    {
        replayed.push(envelope);
    }
    assert!(replayed.iter().all(|e| e.origin == Origin::Replay));
    assert!(replayed.iter().any(|e| matches!(&e.update,
        SessionUpdate::MessageChunk { content, .. } if content.as_text() == Some("from before"))));
    let load = seen
        .lock()
        .unwrap()
        .iter()
        .find(|m| m["method"] == "session/load")
        .cloned()
        .unwrap();
    assert_eq!(load["params"]["sessionId"], "agent-session-1");
}

#[tokio::test]
async fn sign_in_methods_and_both_kinds_of_sign_in() {
    let dir = tempfile::tempdir().unwrap();
    let seen: Arc<Mutex<Vec<Value>>> = Arc::default();
    let agents = manager(dir.path(), &seen);

    let methods = agents.auth_methods("fake").await.unwrap();
    assert_eq!(methods.len(), 2);
    assert!(!methods[0].in_terminal);
    assert!(methods[1].in_terminal);

    // Agent-handled: an ACP authenticate call.
    assert_eq!(agents.authenticate("fake", "chatgpt").await.unwrap(), None);
    assert!(seen
        .lock()
        .unwrap()
        .iter()
        .any(|m| m["method"] == "authenticate" && m["params"]["methodId"] == "chatgpt"));
    // Terminal: the command for the user to run.
    assert_eq!(
        agents
            .authenticate("fake", "setup")
            .await
            .unwrap()
            .as_deref(),
        Some("fake-agent acp --setup")
    );
    assert!(agents.authenticate("fake", "nope").await.is_err());
}

#[tokio::test]
async fn unknown_agent_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let agents = manager(dir.path(), &Arc::default());
    let err = agents.backend("nope").await.unwrap_err();
    assert!(err.to_string().contains("no agent named `nope`"));
}
