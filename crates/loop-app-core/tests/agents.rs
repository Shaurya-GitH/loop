//! Subscriptions with a fake ACP agent: explicit sign-in through the declared
//! method, cached models, sessions on a chosen model, resume and sign-out.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use loop_acp::{AgentConfig, AgentLink, Connector};
use loop_app_core::agents::{find_recorded_session, AgentEntry, AgentManager};
use loop_protocol::{AgentBackend, Origin, SessionUpdate};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

type Seen = Arc<Mutex<Vec<Value>>>;
type Env = Arc<Mutex<Vec<(String, String)>>>;

/// A fake agent that needs a sign-in: answers the ACP calls Loop makes and
/// records them, with the environment it was started with.
#[derive(Default)]
struct FakeAgent {
    seen: Seen,
    env: Env,
}

fn config_options(model: &str, effort: &str) -> Value {
    json!([
        {"id": "model", "name": "Model", "category": "model", "type": "select",
        "currentValue": model,
        "options": [{"value": "gpt-a", "name": "GPT A"}, {"value": "gpt-b", "name": "GPT B"}]},
        {"id": "effort", "name": "Reasoning", "category": "thought_level", "type": "select",
        "currentValue": effort,
        "options": [{"value": "low", "name": "Low"}, {"value": "high", "name": "High"}]}
    ])
}

#[async_trait]
impl Connector for FakeAgent {
    async fn connect(&self, config: &AgentConfig, _cwd: &Path) -> std::io::Result<AgentLink> {
        *self.env.lock().unwrap() = config.env.clone();
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
                let params = &message["params"];
                let session = params["sessionId"].as_str().unwrap_or("").to_owned();
                let mut out = Vec::new();
                let result = match message["method"].as_str().unwrap_or("") {
                    "initialize" => json!({"protocolVersion": 1,
                    "agentCapabilities": {"loadSession": true},
                    "authMethods": [
                        {"id": "api-key", "name": "API Key"},
                        {"id": "chat-gpt", "name": "ChatGPT"},
                        {"id": "chat-gpt-device-code", "name": "ChatGPT (device code)"}
                    ]}),
                    "authenticate" => json!({}),
                    "session/new" => {
                        let n = seen
                            .lock()
                            .unwrap()
                            .iter()
                            .filter(|m| m["method"] == "session/new")
                            .count();
                        json!({"sessionId": format!("s{n}"), "configOptions": config_options("gpt-a", "low")})
                    }
                    "session/set_config_option" => {
                        let value = params["value"].as_str().unwrap_or("");
                        let options = match params["configId"].as_str() {
                            Some("effort") => config_options("gpt-a", value),
                            _ => config_options(value, "low"),
                        };
                        json!({"configOptions": options})
                    }
                    "session/load" => {
                        out.push(
                            json!({"jsonrpc": "2.0", "method": "session/update", "params": {
                            "sessionId": session, "update": {"sessionUpdate": "agent_message_chunk",
                            "content": {"type": "text", "text": "from before"}}}}),
                        );
                        json!({})
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

fn subscription() -> AgentEntry {
    serde_json::from_value(json!({
        "id": "sub", "name": "Sub", "label": "Sub Plus (subscription)",
        "command": "fake-agent", "homeEnv": "FAKE_HOME", "authMethod": "chat-gpt"
    }))
    .unwrap()
}

fn manager(dir: &Path) -> (AgentManager, Seen, Env) {
    manager_with(dir, vec![subscription()])
}

fn manager_with(dir: &Path, entries: Vec<AgentEntry>) -> (AgentManager, Seen, Env) {
    let agent = FakeAgent::default();
    let seen = Arc::clone(&agent.seen);
    let env = Arc::clone(&agent.env);
    let manager = AgentManager::with_connector(entries, dir.to_path_buf(), dir, Arc::new(agent));
    (manager, seen, env)
}

fn sent(seen: &Seen, method: &str) -> Vec<Value> {
    seen.lock()
        .unwrap()
        .iter()
        .filter(|m| m["method"] == method)
        .cloned()
        .collect()
}

#[tokio::test]
async fn sign_in_uses_the_declared_method_and_caches_models() {
    let dir = tempfile::tempdir().unwrap();
    let (agents, seen, env) = manager(dir.path());

    assert!(!agents.is_signed_in("sub"));
    let err = agents.new_session("sub", None).await.unwrap_err();
    assert!(
        err.to_string().contains("/login → Sub Plus (subscription)"),
        "{err}"
    );

    assert_eq!(agents.sign_in("sub").await.unwrap(), 2);
    assert!(agents.is_signed_in("sub"));
    let auth = sent(&seen, "authenticate");
    assert_eq!(auth.len(), 1);
    assert_eq!(
        auth[0]["params"]["methodId"], "chat-gpt",
        "never the API-key method"
    );
    let models: Vec<_> = agents.models("sub").into_iter().map(|m| m.id).collect();
    assert_eq!(models, ["gpt-a", "gpt-b"]);

    let home = dir.path().join("agents").join("sub").join("home");
    assert!(home.is_dir());
    assert!(env
        .lock()
        .unwrap()
        .iter()
        .any(|(k, v)| k == "FAKE_HOME" && Path::new(v) == home));
}

#[tokio::test]
async fn sessions_start_on_the_chosen_model_and_are_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let (agents, seen, _) = manager(dir.path());
    agents.sign_in("sub").await.unwrap();

    let (backend, session) = agents.new_session("sub", Some("gpt-b")).await.unwrap();
    let set = sent(&seen, "session/set_config_option");
    assert_eq!(set.last().unwrap()["params"]["value"], "gpt-b");
    let current = backend
        .model_control()
        .unwrap()
        .models(&session)
        .await
        .unwrap()
        .current;
    assert_eq!(current.as_deref(), Some("gpt-b"));

    let record = find_recorded_session(dir.path(), &session.native_id).unwrap();
    assert_eq!(record.agent, "sub");
    assert!(agents.new_session("sub", Some("gpt-z")).await.is_err());
}

#[tokio::test]
async fn resume_replays_the_recorded_session() {
    let dir = tempfile::tempdir().unwrap();
    let (agents, _, _) = manager(dir.path());
    agents.sign_in("sub").await.unwrap();
    let (_, session) = agents.new_session("sub", None).await.unwrap();
    let record = agents.find_session(&session.native_id).unwrap();

    let backend = agents.backend("sub").await.unwrap();
    let mut updates = backend.subscribe();
    agents.resume(&record).await.unwrap();
    let mut replayed = Vec::new();
    while let Ok(Ok(envelope)) =
        tokio::time::timeout(Duration::from_millis(100), updates.recv()).await
    {
        replayed.push(envelope);
    }
    assert!(replayed.iter().all(|e| e.origin == Origin::Replay));
    assert!(replayed.iter().any(|e| matches!(&e.update,
        SessionUpdate::MessageChunk { content, .. } if content.as_text() == Some("from before"))));
}

#[tokio::test]
async fn new_sessions_reuse_the_remembered_effort() {
    let dir = tempfile::tempdir().unwrap();
    let (agents, seen, _) = manager(dir.path());
    agents.sign_in("sub").await.unwrap();
    assert_eq!(agents.effort("sub"), None);

    agents.remember_effort("sub", "high").unwrap();
    assert_eq!(agents.effort("sub").as_deref(), Some("high"));
    let (backend, session) = agents.new_session("sub", None).await.unwrap();
    let efforts = backend
        .model_control()
        .unwrap()
        .efforts(&session)
        .await
        .unwrap();
    assert_eq!(efforts.current.as_deref(), Some("high"));

    // An effort the model does not offer is left alone.
    agents.remember_effort("sub", "ultra").unwrap();
    let before = sent(&seen, "session/set_config_option").len();
    agents.new_session("sub", None).await.unwrap();
    assert_eq!(sent(&seen, "session/set_config_option").len(), before);
}

#[tokio::test]
async fn sign_out_forgets_everything() {
    let dir = tempfile::tempdir().unwrap();
    let (agents, _, _) = manager(dir.path());
    agents.sign_in("sub").await.unwrap();
    agents.sign_out("sub").await.unwrap();
    assert!(!agents.is_signed_in("sub"));
    assert!(agents.models("sub").is_empty());
    assert!(!dir.path().join("agents").join("sub").exists());
}

#[tokio::test]
async fn unknown_subscription_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let (agents, _, _) = manager(dir.path());
    let err = agents.backend("nope").await.unwrap_err();
    assert!(err.to_string().contains("no subscription"));
    assert!(!agents.is_signed_in("nope"));
}

#[tokio::test]
async fn sign_in_prefers_a_listed_method_the_agent_offers() {
    let entry = |id: &str, preferred: &str| -> AgentEntry {
        serde_json::from_value(json!({
            "id": id, "command": "fake-agent", "authMethod": "chat-gpt",
            "preferredAuthMethods": [preferred]
        }))
        .unwrap()
    };
    let dir = tempfile::tempdir().unwrap();
    let (agents, seen, _) = manager_with(
        dir.path(),
        vec![
            entry("offered", "chat-gpt-device-code"),
            entry("missing", "not-offered"),
        ],
    );
    agents.sign_in("offered").await.unwrap();
    agents.sign_in("missing").await.unwrap();
    let methods: Vec<_> = sent(&seen, "authenticate")
        .iter()
        .map(|m| m["params"]["methodId"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(methods, ["chat-gpt-device-code", "chat-gpt"]);
}
