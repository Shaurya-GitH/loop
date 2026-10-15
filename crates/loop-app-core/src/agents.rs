//! Subscription providers: model access that comes with a subscription
//! (such as a ChatGPT Plus/Pro plan) and is served by an external agent over
//! the Agent Client Protocol.
//!
//! To the user a subscription is one more provider: it is signed in with
//! `/login`, and its models appear in `/model`. Underneath, each subscription
//! is an ACP agent (for ChatGPT, `codex-acp`) that Loop starts on demand.
//!
//! Subscriptions are data. A settings entry with the same id replaces a
//! built-in one; any other entry adds one. Nothing here branches on a vendor.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail, Context};
use async_trait::async_trait;
use loop_acp::schema::v1::AuthMethod;
use loop_acp::{AcpBackend, AgentConfig, Connector, ProcessConnector};
use loop_protocol::{
    AgentBackend, HostServices, InteractionOutcome, InteractionRequest, ModelChoice, SessionKey,
    SessionRef, SessionSpec,
};
use serde::{Deserialize, Serialize};

/// A subscription provider and the ACP agent that serves it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentEntry {
    /// Provider id, used in `provider/model` specs (for example `chatgpt`).
    pub id: String,
    /// Short name, such as "ChatGPT".
    #[serde(default)]
    pub name: String,
    /// The `/login` row, such as "ChatGPT Plus/Pro (Codex subscription)".
    #[serde(default)]
    pub label: String,
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
    /// Environment variable naming the folder where the agent keeps its own
    /// state (for Codex, `CODEX_HOME`). Loop points it at a private folder,
    /// so the agent does not reuse sign-ins from its standalone install.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home_env: Option<String>,
    /// The agent's sign-in method for the subscription (an ACP auth method
    /// id the agent handles itself, for example by opening a browser).
    /// `None` means the agent needs no sign-in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_method: Option<String>,
}

impl AgentEntry {
    /// The short name, falling back to the id.
    pub fn display_name(&self) -> &str {
        if self.name.is_empty() {
            &self.id
        } else {
            &self.name
        }
    }

    /// The `/login` row label, falling back to the name.
    pub fn login_label(&self) -> &str {
        if self.label.is_empty() {
            self.display_name()
        } else {
            &self.label
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

/// Built-in subscriptions.
pub fn builtin_agents() -> Vec<AgentEntry> {
    vec![AgentEntry {
        id: "chatgpt".into(),
        name: "ChatGPT".into(),
        label: "ChatGPT Plus/Pro (Codex subscription)".into(),
        command: "npx".into(),
        args: vec!["-y".into(), "@agentclientprotocol/codex-acp".into()],
        env: BTreeMap::new(),
        install_hint: Some("Needs Node.js (npx).".into()),
        home_env: Some("CODEX_HOME".into()),
        auth_method: Some("chat-gpt".into()),
    }]
}

/// Built-ins, with settings entries replacing those of the same id and
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

/// A session Loop opened with a subscription, kept so `--resume` can find it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSessionRecord {
    /// The agent's session id (also Loop's session key for it).
    pub id: String,
    /// Subscription id.
    pub agent: String,
    /// Working directory the session was opened in.
    pub cwd: PathBuf,
    /// Unix epoch milliseconds.
    pub created_at: i64,
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

/// A model cached from a subscription, shown in `/model`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedModel {
    /// The agent's model id.
    pub id: String,
    /// Display name.
    pub name: String,
}

/// Starts subscription agents on demand and remembers their sessions.
pub struct AgentManager {
    entries: Vec<AgentEntry>,
    cwd: PathBuf,
    index_path: PathBuf,
    /// Per-agent state: private homes, sign-in markers, model caches.
    state_dir: PathBuf,
    connector: Arc<dyn Connector>,
    host: Mutex<Arc<dyn HostServices>>,
    started: tokio::sync::Mutex<HashMap<String, Arc<AcpBackend>>>,
}

impl AgentManager {
    /// A manager for `entries`, starting agents in `cwd` and keeping their
    /// state under `agent_dir`. Until a surface installs its host,
    /// interactions are answered by [`HeadlessHost`].
    pub fn new(entries: Vec<AgentEntry>, cwd: PathBuf, agent_dir: &Path) -> Self {
        Self::with_connector(entries, cwd, agent_dir, Arc::new(ProcessConnector))
    }

    /// A manager that reaches agents through `connector` (tests use a fake).
    pub fn with_connector(
        entries: Vec<AgentEntry>,
        cwd: PathBuf,
        agent_dir: &Path,
        connector: Arc<dyn Connector>,
    ) -> Self {
        Self {
            entries,
            cwd,
            index_path: index_path(agent_dir),
            state_dir: agent_dir.join("agents"),
            connector,
            host: Mutex::new(Arc::new(HeadlessHost)),
            started: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Where agents ask the user. Applies to agents started afterwards.
    pub fn set_host(&self, host: Arc<dyn HostServices>) {
        *self.host.lock().unwrap_or_else(|p| p.into_inner()) = host;
    }

    /// Every subscription Loop knows.
    pub fn entries(&self) -> &[AgentEntry] {
        &self.entries
    }

    /// The subscription `id`, if there is one.
    pub fn entry(&self, id: &str) -> Option<&AgentEntry> {
        self.entries.iter().find(|e| e.id == id)
    }

    /// Whether subscription `id` may be used: signed in through `/login`, or
    /// needing no sign-in.
    pub fn is_signed_in(&self, id: &str) -> bool {
        match self.entry(id) {
            Some(entry) if entry.auth_method.is_some() => self.marker(id).exists(),
            Some(_) => true,
            None => false,
        }
    }

    /// Models cached at sign-in for `id`, for `/model`.
    pub fn models(&self, id: &str) -> Vec<CachedModel> {
        std::fs::read_to_string(self.models_path(id))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// Sign in to subscription `id` with its declared method (the agent may
    /// open a browser), then read and cache its models. Returns how many
    /// models it offers.
    pub async fn sign_in(&self, id: &str) -> anyhow::Result<usize> {
        let entry = self
            .entry(id)
            .ok_or_else(|| anyhow!("no subscription named `{id}`"))?;
        let backend = self.backend(id).await?;
        if let Some(method) = &entry.auth_method {
            let offered = backend.auth_methods().into_iter().any(|m| match m {
                AuthMethod::Agent(m) => m.id.0.as_ref() == method.as_str(),
                _ => false,
            });
            if !offered {
                bail!(
                    "{} no longer offers the `{method}` sign-in; update Loop",
                    entry.display_name()
                );
            }
            backend.authenticate(method).await?;
            self.mark_signed_in(id)?;
        }
        // A short-lived session, only to learn the models.
        let probe = backend
            .new_session(SessionSpec {
                cwd: Some(self.cwd.clone()),
                name: None,
            })
            .await?;
        self.refresh_models(id, backend.as_ref(), &probe).await
    }

    /// Forget the sign-in for `id`: stop the agent, delete its private home
    /// (where it keeps its credentials), the marker and the model cache.
    pub async fn sign_out(&self, id: &str) -> anyhow::Result<()> {
        if let Some(backend) = self.started.lock().await.remove(id) {
            backend.shutdown().await;
        }
        let dir = self.state_dir.join(id);
        if dir.exists() {
            std::fs::remove_dir_all(&dir).with_context(|| format!("remove {}", dir.display()))?;
        }
        Ok(())
    }

    /// The running backend for `id`, starting the agent if needed.
    pub async fn backend(&self, id: &str) -> anyhow::Result<Arc<AcpBackend>> {
        let entry = self
            .entry(id)
            .ok_or_else(|| anyhow!("no subscription named `{id}`"))?;
        let mut started = self.started.lock().await;
        if let Some(backend) = started.get(id) {
            return Ok(Arc::clone(backend));
        }
        let host = Arc::clone(&self.host.lock().unwrap_or_else(|p| p.into_inner()));
        let mut config = entry.config();
        if let Some(var) = &entry.home_env {
            let home = self.state_dir.join(&entry.id).join("home");
            std::fs::create_dir_all(&home).with_context(|| format!("create {}", home.display()))?;
            config
                .env
                .push((var.clone(), home.to_string_lossy().into_owned()));
        }
        let backend =
            AcpBackend::start(config, self.cwd.clone(), Arc::clone(&self.connector), host)
                .await
                .map_err(|e| match &entry.install_hint {
                    Some(hint) => anyhow!("{e}. {hint}"),
                    None => anyhow!("{e}"),
                })?;
        started.insert(id.to_owned(), Arc::clone(&backend));
        Ok(backend)
    }

    /// Open a new session with subscription `id`, switch it to `model` when
    /// given, and remember it for `--resume`.
    pub async fn new_session(
        &self,
        id: &str,
        model: Option<&str>,
    ) -> anyhow::Result<(Arc<AcpBackend>, SessionRef)> {
        self.require_sign_in(id)?;
        let backend = self.backend(id).await?;
        let session = backend
            .new_session(SessionSpec {
                cwd: Some(self.cwd.clone()),
                name: None,
            })
            .await?;
        // Keep the cache current; a failure here is not worth failing the session.
        let _ = self.refresh_models(id, backend.as_ref(), &session).await;
        if let Some(model) = model {
            let control = backend
                .model_control()
                .ok_or_else(|| anyhow!("{id} does not let Loop choose its model"))?;
            control.set_model(&session, model).await?;
        }
        // Carry the last chosen effort over when this model offers it.
        if let (Some(effort), Some(control)) = (self.effort(id), backend.model_control()) {
            let offered = control
                .efforts(&session)
                .await
                .is_ok_and(|e| e.models.iter().any(|m| m.id == effort));
            if offered {
                let _ = control.set_effort(&session, &effort).await;
            }
        }
        self.record(AgentSessionRecord {
            id: session.native_id.clone(),
            agent: id.to_owned(),
            cwd: self.cwd.clone(),
            created_at: now_ms(),
        })?;
        Ok((backend, session))
    }

    /// The recorded session `session_id`, if Loop opened one.
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
        self.require_sign_in(&record.agent)?;
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

    /// Stop every running agent.
    pub async fn shutdown(&self) {
        let started: Vec<_> = self.started.lock().await.drain().map(|(_, b)| b).collect();
        for backend in started {
            backend.shutdown().await;
        }
    }

    /// Record that subscription `id` was signed in through Loop.
    pub fn mark_signed_in(&self, id: &str) -> anyhow::Result<()> {
        let marker = self.marker(id);
        if let Some(dir) = marker.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&marker, now_ms().to_string())?;
        Ok(())
    }

    async fn refresh_models(
        &self,
        id: &str,
        backend: &dyn AgentBackend,
        session: &SessionRef,
    ) -> anyhow::Result<usize> {
        let Some(control) = backend.model_control() else {
            return Ok(0);
        };
        let models: Vec<CachedModel> = control
            .models(session)
            .await?
            .models
            .into_iter()
            .map(|ModelChoice { id, name, .. }| CachedModel { id, name })
            .collect();
        let path = self.models_path(id);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, serde_json::to_vec_pretty(&models)?)?;
        Ok(models.len())
    }

    fn require_sign_in(&self, id: &str) -> anyhow::Result<()> {
        if self.entry(id).is_none() {
            bail!("no subscription named `{id}`");
        }
        if self.is_signed_in(id) {
            return Ok(());
        }
        let label = self.entry(id).map(AgentEntry::login_label).unwrap_or(id);
        bail!("sign in first: /login → {label}")
    }

    fn marker(&self, id: &str) -> PathBuf {
        self.state_dir.join(id).join("signed-in")
    }

    /// The reasoning effort last chosen for subscription `id`.
    pub fn effort(&self, id: &str) -> Option<String> {
        let text = std::fs::read_to_string(self.effort_path(id)).ok()?;
        Some(text.trim().to_owned()).filter(|e| !e.is_empty())
    }

    /// Save `effort` for the next session with subscription `id`.
    pub fn remember_effort(&self, id: &str, effort: &str) -> anyhow::Result<()> {
        let path = self.effort_path(id);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, effort)?;
        Ok(())
    }

    fn effort_path(&self, id: &str) -> PathBuf {
        self.state_dir.join(id).join("effort")
    }

    fn models_path(&self, id: &str) -> PathBuf {
        self.state_dir.join(id).join("models.json")
    }

    fn record(&self, record: AgentSessionRecord) -> anyhow::Result<()> {
        let mut records = read_index(&self.index_path).unwrap_or_default();
        records.retain(|r| r.id != record.id);
        records.push(record);
        write_index(&self.index_path, &records)
    }
}

/// The recorded subscription session `session_id` in `agent_dir`, if Loop
/// opened one there. Lets `--resume` tell these sessions from its own.
pub fn find_recorded_session(agent_dir: &Path, session_id: &str) -> Option<AgentSessionRecord> {
    read_index(&index_path(agent_dir))
        .ok()?
        .into_iter()
        .find(|record| record.id == session_id)
}

/// Subscription ids known in `agent_dir`: built-ins plus global settings.
/// Lets the CLI tell `--provider chatgpt` from Loop's own providers before
/// the runtime starts.
pub fn subscription_ids(agent_dir: &Path) -> Vec<String> {
    let path = crate::config::paths::settings_path(agent_dir);
    let configured = crate::config::settings::Settings::load_file(&path)
        .map(|settings| settings.agents)
        .unwrap_or_default();
    agent_entries(&configured)
        .into_iter()
        .map(|e| e.id)
        .collect()
}

/// Where subscription sessions are recorded.
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
    fn chatgpt_is_the_builtin_subscription() {
        let builtins = builtin_agents();
        assert_eq!(builtins.len(), 1);
        let chatgpt = &builtins[0];
        assert_eq!(chatgpt.id, "chatgpt");
        assert_eq!(
            chatgpt.login_label(),
            "ChatGPT Plus/Pro (Codex subscription)"
        );
        assert_eq!(chatgpt.auth_method.as_deref(), Some("chat-gpt"));
        assert_eq!(chatgpt.home_env.as_deref(), Some("CODEX_HOME"));
    }

    #[test]
    fn settings_replace_and_extend_builtins() {
        let mut chatgpt = builtin_agents()[0].clone();
        chatgpt.args = vec!["--custom".into()];
        let mine: AgentEntry = serde_json::from_value(serde_json::json!({
            "id": "mine", "command": "/opt/mine", "env": {"PROFILE": "work"}
        }))
        .unwrap();
        let entries = agent_entries(&[chatgpt, mine]);
        assert_eq!(entries[0].args, ["--custom"]);
        let mine = entries.last().unwrap();
        assert_eq!(mine.login_label(), "mine");
        assert!(mine.auth_method.is_none());
        assert_eq!(
            mine.config().env,
            [("PROFILE".to_owned(), "work".to_owned())]
        );
    }

    #[test]
    fn session_index_round_trips_and_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let manager = AgentManager::new(Vec::new(), dir.path().into(), dir.path());
        let record = |agent: &str| AgentSessionRecord {
            id: "s1".into(),
            agent: agent.into(),
            cwd: dir.path().into(),
            created_at: 1,
        };
        manager.record(record("chatgpt")).unwrap();
        manager.record(record("other")).unwrap();
        assert_eq!(manager.find_session("s1").unwrap().agent, "other");
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
