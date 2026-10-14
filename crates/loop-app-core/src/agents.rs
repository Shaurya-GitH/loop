//! External agents: the registry of ACP agents Loop can run, and the manager
//! that starts them and remembers their sessions.
//!
//! Agents are data. Built-in presets cover common ACP agents; a settings
//! entry with the same id replaces a preset, and any other entry adds an
//! agent. Nothing here branches on a particular vendor.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail, Context};
use async_trait::async_trait;
use loop_acp::schema::v1::AuthMethod;
use loop_acp::{AcpBackend, AgentConfig, Connector, ProcessConnector};
use loop_protocol::{
    AgentBackend, HostServices, InteractionOutcome, InteractionRequest, SessionKey, SessionRef,
    SessionSpec,
};
use serde::{Deserialize, Serialize};

/// One agent Loop can run over the Agent Client Protocol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentEntry {
    /// Short id used in `/agent <id>` and `--agent <id>`.
    pub id: String,
    /// Display name.
    #[serde(default)]
    pub name: String,
    /// Program to run.
    pub command: String,
    /// Arguments.
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment for the agent. Loop's own API keys are never passed
    /// unless listed here.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// What to install when the command is missing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_hint: Option<String>,
}

impl AgentEntry {
    fn preset(id: &str, name: &str, command: &str, args: &[&str], install_hint: &str) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            command: command.into(),
            args: args.iter().map(|a| (*a).into()).collect(),
            env: BTreeMap::new(),
            install_hint: Some(install_hint.into()),
        }
    }

    /// The display name, falling back to the id.
    pub fn display_name(&self) -> &str {
        if self.name.is_empty() {
            &self.id
        } else {
            &self.name
        }
    }

    /// The launch config for this agent.
    pub fn config(&self) -> AgentConfig {
        let mut config = AgentConfig::new(self.id.as_str(), self.command.as_str());
        config.name = self.display_name().to_owned();
        config.args = self.args.clone();
        config.env = self
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        config
    }
}

/// Built-in agents. Each command is the one its project documents for ACP.
pub fn builtin_agents() -> Vec<AgentEntry> {
    vec![
        AgentEntry::preset(
            "codex",
            "Codex (ChatGPT plan or OpenAI API key)",
            "npx",
            &["-y", "@agentclientprotocol/codex-acp"],
            "Needs Node.js (npx). Signs in with your ChatGPT plan through `/agent login`.",
        ),
        AgentEntry::preset(
            "claude",
            "Claude Agent",
            "npx",
            &["-y", "@agentclientprotocol/claude-agent-acp"],
            "Needs Node.js (npx).",
        ),
        AgentEntry::preset(
            "gemini",
            "Gemini CLI",
            "gemini",
            &["--acp"],
            "Install Gemini CLI: npm install -g @google/gemini-cli",
        ),
        AgentEntry::preset(
            "hermes",
            "Hermes Agent (Nous Research)",
            "hermes",
            &["acp"],
            "Install Hermes Agent and run `hermes model` once to choose a provider.",
        ),
        AgentEntry::preset(
            "opencode",
            "OpenCode",
            "opencode",
            &["acp"],
            "Install OpenCode: https://opencode.ai",
        ),
    ]
}

/// Presets, with settings entries replacing presets of the same id and
/// adding the rest, in a stable order.
pub fn agent_entries(configured: &[AgentEntry]) -> Vec<AgentEntry> {
    let mut entries = builtin_agents();
    for entry in configured {
        match entries.iter_mut().find(|e| e.id == entry.id) {
            Some(existing) => *existing = entry.clone(),
            None => entries.push(entry.clone()),
        }
    }
    entries
}

/// A session Loop opened with an external agent, kept so `--resume` can find
/// it again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSessionRecord {
    /// The agent's session id (also Loop's session key for it).
    pub id: String,
    /// Agent id.
    pub agent: String,
    /// Working directory the session was opened in.
    pub cwd: PathBuf,
    /// Unix epoch milliseconds.
    pub created_at: i64,
}

/// One way to sign in to an agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignInMethod {
    /// The agent's id for the method.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Whether the user runs a command in a terminal (instead of the agent
    /// handling it, for example by opening a browser).
    pub in_terminal: bool,
}

/// Answers every interaction "rejected": used when no surface can ask the
/// user, for example in `--print`. Agents then decline the action.
pub struct HeadlessHost;

#[async_trait]
impl HostServices for HeadlessHost {
    async fn interact(&self, _request: InteractionRequest) -> InteractionOutcome {
        InteractionOutcome::Rejected {
            reason: "no one is available to approve this in a non-interactive run".into(),
        }
    }
}

/// Starts external agents on demand and remembers their sessions.
pub struct AgentManager {
    entries: Vec<AgentEntry>,
    cwd: PathBuf,
    index_path: PathBuf,
    connector: Arc<dyn Connector>,
    host: Mutex<Arc<dyn HostServices>>,
    started: tokio::sync::Mutex<HashMap<String, Arc<AcpBackend>>>,
}

impl AgentManager {
    /// A manager for `entries`, starting agents in `cwd` and recording
    /// sessions in `index_path`. Until a surface installs its host,
    /// interactions are answered by [`HeadlessHost`].
    pub fn new(entries: Vec<AgentEntry>, cwd: PathBuf, index_path: PathBuf) -> Self {
        Self::with_connector(entries, cwd, index_path, Arc::new(ProcessConnector))
    }

    /// A manager that reaches agents through `connector` (tests use a fake).
    pub fn with_connector(
        entries: Vec<AgentEntry>,
        cwd: PathBuf,
        index_path: PathBuf,
        connector: Arc<dyn Connector>,
    ) -> Self {
        Self {
            entries,
            cwd,
            index_path,
            connector,
            host: Mutex::new(Arc::new(HeadlessHost)),
            started: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Where agents ask the user. Applies to agents started afterwards.
    pub fn set_host(&self, host: Arc<dyn HostServices>) {
        *self.host.lock().unwrap_or_else(|p| p.into_inner()) = host;
    }

    /// Every agent Loop can run.
    pub fn entries(&self) -> &[AgentEntry] {
        &self.entries
    }

    /// The registry entry for `id`.
    pub fn entry(&self, id: &str) -> Option<&AgentEntry> {
        self.entries.iter().find(|e| e.id == id)
    }

    /// The running backend for `id`, starting the agent if needed.
    pub async fn backend(&self, id: &str) -> anyhow::Result<Arc<AcpBackend>> {
        let entry = self
            .entry(id)
            .ok_or_else(|| anyhow!("no agent named `{id}` (see /agent)"))?;
        let mut started = self.started.lock().await;
        if let Some(backend) = started.get(id) {
            return Ok(Arc::clone(backend));
        }
        let host = Arc::clone(&self.host.lock().unwrap_or_else(|p| p.into_inner()));
        let backend = AcpBackend::start(
            entry.config(),
            self.cwd.clone(),
            Arc::clone(&self.connector),
            host,
        )
        .await
        .map_err(|e| match &entry.install_hint {
            Some(hint) => anyhow!("{e}. {hint}"),
            None => anyhow!("{e}"),
        })?;
        started.insert(id.to_owned(), Arc::clone(&backend));
        Ok(backend)
    }

    /// Open a new session with agent `id` and remember it for `--resume`.
    pub async fn new_session(&self, id: &str) -> anyhow::Result<(Arc<AcpBackend>, SessionRef)> {
        let backend = self.backend(id).await?;
        let session = backend
            .new_session(SessionSpec {
                cwd: Some(self.cwd.clone()),
                name: None,
            })
            .await?;
        self.record(AgentSessionRecord {
            id: session.native_id.clone(),
            agent: id.to_owned(),
            cwd: self.cwd.clone(),
            created_at: now_ms(),
        })?;
        Ok((backend, session))
    }

    /// The recorded external-agent session `session_id`, if Loop opened one.
    pub fn find_session(&self, session_id: &str) -> Option<AgentSessionRecord> {
        read_index(&self.index_path)
            .ok()?
            .into_iter()
            .find(|record| record.id == session_id)
    }

    /// Reopen a recorded session. Its history arrives on the backend's
    /// update stream as replay; subscribe before calling.
    pub async fn resume(
        &self,
        record: &AgentSessionRecord,
    ) -> anyhow::Result<(Arc<AcpBackend>, SessionRef)> {
        let backend = self.backend(&record.agent).await?;
        let session = SessionRef {
            instance: backend.instance().clone(),
            key: SessionKey(record.id.clone()),
            native_id: record.id.clone(),
        };
        backend
            .load_session(
                &session,
                SessionSpec {
                    cwd: Some(record.cwd.clone()),
                    name: None,
                },
            )
            .await
            .with_context(|| format!("{} cannot reopen this session", record.agent))?;
        Ok((backend, session))
    }

    /// The agent's sign-in methods.
    pub async fn auth_methods(&self, id: &str) -> anyhow::Result<Vec<SignInMethod>> {
        let backend = self.backend(id).await?;
        Ok(backend
            .auth_methods()
            .into_iter()
            .filter_map(|method| match method {
                AuthMethod::Agent(m) => Some(SignInMethod {
                    id: m.id.0.to_string(),
                    name: m.name,
                    in_terminal: false,
                }),
                AuthMethod::Terminal(m) => Some(SignInMethod {
                    id: m.id.0.to_string(),
                    name: m.name,
                    in_terminal: true,
                }),
                _ => None,
            })
            .collect())
    }

    /// Sign in to agent `id` with `method`. Agent-handled methods run through
    /// ACP `authenticate`; terminal methods return the command to run.
    pub async fn authenticate(&self, id: &str, method: &str) -> anyhow::Result<Option<String>> {
        let backend = self.backend(id).await?;
        let Some(auth) = backend.auth_methods().into_iter().find(|m| match m {
            AuthMethod::Agent(m) => m.id.0.as_ref() == method,
            AuthMethod::Terminal(m) => m.id.0.as_ref() == method,
            _ => false,
        }) else {
            bail!("{id} has no sign-in method `{method}`");
        };
        match auth {
            AuthMethod::Terminal(m) => {
                let entry = self
                    .entry(id)
                    .ok_or_else(|| anyhow!("unknown agent {id}"))?;
                let mut command = vec![entry.command.clone()];
                command.extend(entry.args.iter().cloned());
                command.extend(m.args);
                Ok(Some(command.join(" ")))
            }
            _ => {
                backend.authenticate(method).await?;
                Ok(None)
            }
        }
    }

    /// Stop every running agent.
    pub async fn shutdown(&self) {
        let started: Vec<_> = self.started.lock().await.drain().map(|(_, b)| b).collect();
        for backend in started {
            backend.shutdown().await;
        }
    }

    fn record(&self, record: AgentSessionRecord) -> anyhow::Result<()> {
        let mut records = read_index(&self.index_path).unwrap_or_default();
        records.retain(|r| r.id != record.id);
        records.push(record);
        write_index(&self.index_path, &records)
    }
}

/// The recorded external-agent session `session_id` in `agent_dir`, if Loop
/// opened one there. Lets `--resume` tell agent sessions from its own.
pub fn find_recorded_session(agent_dir: &Path, session_id: &str) -> Option<AgentSessionRecord> {
    read_index(&index_path(agent_dir))
        .ok()?
        .into_iter()
        .find(|record| record.id == session_id)
}

/// Where external-agent sessions are recorded.
pub fn index_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join("agent-sessions.json")
}

fn read_index(path: &Path) -> anyhow::Result<Vec<AgentSessionRecord>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(serde_json::from_str(&text)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e.into()),
    }
}

fn write_index(path: &Path, records: &[AgentSessionRecord]) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(records)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_replace_and_extend_presets() {
        let mut codex = builtin_agents()[0].clone();
        codex.args = vec!["--custom".into()];
        let mine = AgentEntry {
            id: "mine".into(),
            name: String::new(),
            command: "/opt/mine".into(),
            args: vec![],
            env: BTreeMap::from([("PROFILE".into(), "work".into())]),
            install_hint: None,
        };
        let entries = agent_entries(&[codex, mine]);
        assert_eq!(entries[0].id, "codex");
        assert_eq!(entries[0].args, ["--custom"]);
        assert_eq!(entries.last().unwrap().id, "mine");
        assert_eq!(entries.last().unwrap().display_name(), "mine");
        let config = entries.last().unwrap().config();
        assert_eq!(config.env, [("PROFILE".to_owned(), "work".to_owned())]);
        assert_eq!(entries.len(), builtin_agents().len() + 1);
    }

    #[test]
    fn settings_entries_parse_from_camel_case_json() {
        let entry: AgentEntry = serde_json::from_value(serde_json::json!({
            "id": "kimi", "command": "kimi", "args": ["--acp"], "installHint": "pip install kimi-cli"
        }))
        .unwrap();
        assert_eq!(entry.install_hint.as_deref(), Some("pip install kimi-cli"));
        assert!(entry.env.is_empty());
    }

    #[test]
    fn session_index_round_trips_and_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let manager = AgentManager::new(
            Vec::new(),
            dir.path().into(),
            dir.path().join("agent-sessions.json"),
        );
        let record = |agent: &str| AgentSessionRecord {
            id: "s1".into(),
            agent: agent.into(),
            cwd: dir.path().into(),
            created_at: 1,
        };
        manager.record(record("codex")).unwrap();
        manager.record(record("gemini")).unwrap();
        assert_eq!(manager.find_session("s1").unwrap().agent, "gemini");
        assert!(manager.find_session("nope").is_none());
    }

    #[tokio::test]
    async fn headless_host_rejects() {
        let outcome = HeadlessHost
            .interact(InteractionRequest::Url {
                id: "u".into(),
                url: "https://example.com".into(),
                purpose: "sign in".into(),
            })
            .await;
        assert!(matches!(outcome, InteractionOutcome::Rejected { .. }));
    }
}
