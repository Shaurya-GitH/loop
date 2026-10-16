//! Bootstrap Models, sessions, harness, and first-run auth.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::bail;

use loop_agent::harness::{
    create_bash_tool, create_edit_tool, create_read_tool, create_session_repository,
    create_sqlite_session_store, create_write_tool, AgentHarness, AgentHarnessOptions,
    AgentHarnessResources, HostExecutionEnv, KrunIsolation, KrunSandbox, LocalSandboxRuntime,
    Sandbox, SandboxMode,
};
use loop_agent::types::{AgentThinkingLevel, AgentTool};
use loop_ai::providers::{custom_provider, CustomModelSpec, CustomProviderConfig};
use loop_ai::{CreateModelsOptions, FileModelsStore, Models, ModelsRefreshOptions};

use crate::config::auth::FileCredentialStore;
use crate::config::paths::trust_path;
use crate::config::paths::{
    auth_path, ensure_agent_dirs, get_agent_dir, models_json_path, models_store_path,
    sessions_db_path, settings_path,
};
use crate::config::settings::{
    load_settings, set_disabled_name, set_skill_disabled, McpServerConfig, Settings,
};
use crate::config::trust::TrustStore;
use crate::model_selection::StartupModel;
use crate::resources::{load_resources, LoadedResources};
use crate::system_prompt::{
    build_system_prompt, default_tool_snippets, load_context_files, resolve_system_prompt_files,
    BuildSystemPromptOptions,
};
use crate::theme::{theme_search_dirs, Theme};
use loop_ai::CredentialStore;

/// Fully constructed interactive runtime.
pub struct Runtime {
    /// Agent config dir.
    pub agent_dir: PathBuf,
    /// Cwd.
    pub cwd: PathBuf,
    /// Settings.
    pub settings: Settings,
    /// Models collection.
    pub models: Arc<Models>,
    /// Credential store.
    pub credentials: Arc<FileCredentialStore>,
    /// Harness.
    pub harness: Arc<AgentHarness>,
    /// The agent prompts go to. Surfaces talk to it only through
    /// [`loop_protocol::AgentBackend`].
    pub backend: Arc<dyn loop_protocol::AgentBackend>,
    /// Loop's own agent, for native-only features (sandbox, skills, MCP,
    /// forking, workflows). Same harness as `harness`.
    pub native: Arc<loop_agent::native_backend::NativeBackend>,
    /// External ACP agents Loop can switch to.
    pub agents: Arc<crate::agents::AgentManager>,
    /// The external agent prompts go to, or `None` for Loop's own agent.
    pub active_agent: Option<String>,
    /// The model chosen for the active subscription, when one was chosen.
    pub agent_model: Option<String>,
    /// The active subscription's reasoning effort, when it has one.
    pub agent_effort: Option<String>,
    /// Theme.
    pub theme: Theme,
    /// Resources.
    pub resources: LoadedResources,
    /// Project trusted.
    pub project_trusted: bool,
    /// Trust store.
    pub trust: TrustStore,
    /// Session store path.
    pub sessions_db: PathBuf,
    /// Active session id (persisted in SQLite; sent to the provider API).
    pub session_id: String,
    /// True when started with `--resume <id>` (transcript hydrated from store).
    pub resumed: bool,
    /// When true, the TUI opens the first-run provider setup (`/login`).
    pub needs_provider_setup: bool,
    /// The model prompts go to (mirrors the harness; change it via [`Runtime::select_model`]).
    pub selected_model: Option<loop_ai::Model>,
    /// Why no model is selected, when a saved one could not be used.
    pub model_note: Option<String>,
    /// Interactive tool approval bridge (set by the TUI).
    pub tool_approval: Option<std::sync::Arc<crate::tool_approval::ToolApprovalBridge>>,
    /// MCP client manager for external tool servers.
    pub mcp_client: Arc<loop_mcp::McpClientManager>,
    /// Skills activated via `/skill:name` (not yet cleared; mirrored on the harness).
    pub active_skills: Vec<String>,
    /// Process telemetry, when the host application installed it.
    #[cfg(feature = "telemetry")]
    pub telemetry: Option<loop_telemetry::TelemetryHandle>,
}

impl Runtime {
    /// Whether prompts go to Loop's own agent.
    pub fn is_native(&self) -> bool {
        self.active_agent.is_none()
    }

    /// Whether `provider` is a subscription served by an external agent.
    pub fn is_subscription(&self, provider: &str) -> bool {
        self.agents.entry(provider).is_some()
    }

    /// Make `backend`'s `session` the active one, for subscription `id`
    /// running `model`.
    pub fn adopt_agent(
        &mut self,
        id: &str,
        backend: Arc<dyn loop_protocol::AgentBackend>,
        session: loop_protocol::SessionRef,
        model: Option<String>,
    ) {
        self.backend = backend;
        self.session_id = session.native_id;
        self.active_agent = Some(id.to_owned());
        self.agent_model = model;
        self.agent_effort = None;
    }

    /// Read the active subscription's current reasoning effort into
    /// [`Self::agent_effort`], and its model into [`Self::agent_model`] when
    /// Loop did not choose one (a resumed session).
    pub async fn sync_agent_effort(&mut self) {
        let session = self.session_ref();
        let Some(control) = self.backend.model_control().filter(|_| !self.is_native()) else {
            self.agent_effort = None;
            return;
        };
        self.agent_effort = control.efforts(&session).await.ok().and_then(|e| e.current);
        if self.agent_model.is_none() {
            self.agent_model = control.models(&session).await.ok().and_then(|m| m.current);
        }
    }

    /// The reasoning effort Loop's own agent uses with the selected model:
    /// the saved level, or `off` when the model does not accept it.
    pub fn native_effort(&self) -> String {
        let saved = self.settings.default_thinking_level.to_lowercase();
        if native_efforts(self.selected_model.as_ref(), &NATIVE_EFFORTS).contains(&saved) {
            saved
        } else {
            "off".into()
        }
    }

    /// The reasoning effort levels on offer, and the current one.
    pub async fn efforts(&self) -> anyhow::Result<(Vec<String>, String)> {
        if self.is_native() {
            let current = self.native_effort();
            return Ok((
                native_efforts(self.selected_model.as_ref(), &NATIVE_EFFORTS),
                current,
            ));
        }
        let control = self
            .backend
            .model_control()
            .ok_or_else(|| anyhow::anyhow!("this subscription has no reasoning effort setting"))?;
        let choices = control.efforts(&self.session_ref()).await?;
        let levels = choices.models.into_iter().map(|m| m.id).collect();
        Ok((levels, choices.current.unwrap_or_default()))
    }

    /// Set the reasoning effort to `level` and remember it for the next
    /// session. Returns the level set.
    pub async fn set_effort(&mut self, level: &str) -> anyhow::Result<String> {
        let level = level.trim().to_lowercase();
        let (levels, _) = self.efforts().await?;
        if !levels.contains(&level) {
            anyhow::bail!(
                "unknown effort `{level}`; choose one of: {}",
                levels.join(", ")
            );
        }
        match self.active_agent.clone() {
            None => {
                self.harness
                    .set_thinking_level(parse_thinking(&level))
                    .await;
                self.settings.default_thinking_level = level.clone();
                self.save_settings()?;
            }
            Some(id) => {
                if let Some(control) = self.backend.model_control() {
                    control.set_effort(&self.session_ref(), &level).await?;
                }
                self.agents.remember_effort(&id, &level)?;
                self.agent_effort = Some(level.clone());
            }
        }
        Ok(level)
    }

    /// Step to the next reasoning effort (wrapping). Loop's own agent cycles
    /// off → low → medium → high.
    pub async fn cycle_effort(&mut self) -> anyhow::Result<String> {
        let (levels, current) = if self.is_native() {
            let levels = native_efforts(
                self.selected_model.as_ref(),
                &["off", "low", "medium", "high"],
            );
            (levels, self.native_effort())
        } else {
            self.efforts().await?
        };
        let next = levels
            .iter()
            .position(|l| *l == current)
            .map_or(0, |i| (i + 1) % levels.len());
        let level = levels
            .get(next)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no reasoning effort levels on offer"))?;
        self.set_effort(&level).await
    }

    /// Start a new session with subscription `id` (on `model`, when given)
    /// and make it active.
    pub async fn use_agent(&mut self, id: &str, model: Option<&str>) -> anyhow::Result<()> {
        let (backend, session) = self.agents.new_session(id, model).await?;
        self.adopt_agent(id, backend, session, model.map(str::to_owned));
        self.sync_agent_effort().await;
        Ok(())
    }

    /// Save `provider/model` as the default for the next start.
    pub fn remember_model(&mut self, provider: &str, model: &str) -> anyhow::Result<()> {
        self.settings.set_selected_model(provider, model);
        self.save_settings()
    }

    /// `provider/model` of what prompts go to, with the kind of access.
    pub fn active_model_label(&self) -> Option<String> {
        match (&self.active_agent, &self.agent_model) {
            (Some(agent), model) => {
                let mut label = agent.clone();
                if let Some(model) = model {
                    label = format!("{label}/{model}");
                }
                if let Some(effort) = &self.agent_effort {
                    label = format!("{label} · {effort}");
                }
                Some(format!("{label} · subscription"))
            }
            (None, _) => self.selected_model_spec(),
        }
    }

    /// Reopen a recorded external-agent session and make it active. The
    /// session id is set first, so replayed history routes to it.
    pub async fn resume_agent(
        &mut self,
        record: &crate::agents::AgentSessionRecord,
    ) -> anyhow::Result<()> {
        let previous = std::mem::replace(&mut self.session_id, record.id.clone());
        match self.agents.resume(record).await {
            Ok((backend, session)) => {
                self.adopt_agent(&record.agent, backend, session, None);
                self.sync_agent_effort().await;
                Ok(())
            }
            Err(e) => {
                self.session_id = previous;
                Err(e)
            }
        }
    }

    /// Switch prompts back to Loop's own agent and its current session.
    pub async fn use_native(&mut self) {
        self.backend = self.native.clone();
        self.session_id = self.harness.session_id().await;
        self.active_agent = None;
        self.agent_model = None;
        self.agent_effort = None;
    }

    /// Start a new session with the active agent.
    pub async fn new_session(&mut self) -> anyhow::Result<()> {
        match self.active_agent.clone() {
            Some(id) => {
                let model = self.agent_model.clone();
                self.use_agent(&id, model.as_deref()).await
            }
            None => {
                let session = self
                    .backend
                    .new_session(loop_protocol::SessionSpec {
                        cwd: Some(self.cwd.clone()),
                        name: None,
                    })
                    .await?;
                self.session_id = session.native_id;
                Ok(())
            }
        }
    }

    /// The active session as the backend interface names it.
    pub fn session_ref(&self) -> loop_protocol::SessionRef {
        loop_protocol::SessionRef {
            instance: self.backend.instance().clone(),
            key: loop_protocol::SessionKey(self.session_id.clone()),
            native_id: self.session_id.clone(),
        }
    }

    /// Persist the current settings to the global settings file.
    pub fn save_settings(&self) -> anyhow::Result<()> {
        self.settings.save_file(&settings_path(&self.agent_dir))
    }

    /// Push the loaded skills and prompt templates to the harness.
    pub async fn sync_harness_resources(&self) {
        self.harness
            .set_resources(AgentHarnessResources {
                skills: self.resources.skills.clone(),
                prompt_templates: self.resources.prompts.clone(),
            })
            .await;
    }

    /// Enable or disable a discovered skill for this session and in the global settings file.
    ///
    /// Disabling also deactivates the skill if it was turned on with `/skill:name`.
    /// Returns `Ok(false)` when no skill has that name.
    pub async fn set_skill_enabled(&mut self, name: &str, enabled: bool) -> anyhow::Result<bool> {
        if !self.resources.set_skill_enabled(name, enabled) {
            return Ok(false);
        }
        set_skill_disabled(&mut self.settings.disabled_skills, name, enabled);
        if !enabled && self.active_skills.iter().any(|n| n == name) {
            self.active_skills.retain(|n| n != name);
            self.harness.clear_active_skills().await;
        }
        self.sync_harness_resources().await;
        for active in &self.active_skills {
            self.harness.activate_skill(active).await;
        }

        // Edit only the global file so project overrides are not copied into it.
        let path = settings_path(&self.agent_dir);
        let mut global = Settings::load_file(&path)?;
        set_skill_disabled(&mut global.disabled_skills, name, enabled);
        global.save_file(&path)?;
        Ok(true)
    }

    /// Enable or disable a configured MCP server for this session and in the global settings file.
    ///
    /// Disabling disconnects it and drops its tools. Enabling connects it and registers its tools.
    /// [`McpToggleOutcome::Missing`] means no server has that name. A connect failure is reported
    /// after the enabled state has already been saved. `Err` means saving settings failed.
    pub async fn set_mcp_server_enabled(
        &mut self,
        name: &str,
        enabled: bool,
    ) -> anyhow::Result<McpToggleOutcome> {
        if !self.settings.mcp_servers.contains_key(name) {
            return Ok(McpToggleOutcome::Missing);
        }
        set_disabled_name(&mut self.settings.disabled_mcp_servers, name, enabled);
        let path = settings_path(&self.agent_dir);
        let mut global = Settings::load_file(&path)?;
        set_disabled_name(&mut global.disabled_mcp_servers, name, enabled);
        global.save_file(&path)?;

        let connect_error = if enabled {
            let cfg = self.settings.mcp_servers.get(name).cloned();
            if let Some(cfg) = cfg {
                let mut one = std::collections::BTreeMap::new();
                one.insert(name.to_string(), cfg);
                match enabled_mcp_server_entries(&one, &[]).into_iter().next() {
                    Some(entry) => self.mcp_client.connect(&entry).await.err(),
                    None => Some(format!("mcp server '{name}' has neither command nor url")),
                }
            } else {
                None
            }
        } else {
            self.mcp_client.disconnect(name).await;
            None
        };
        self.refresh_mcp_tools().await?;
        Ok(match connect_error {
            Some(err) => McpToggleOutcome::ConnectFailed(err),
            None => McpToggleOutcome::Updated,
        })
    }

    /// Replace harness `mcp__*` tools with the ones from currently connected servers.
    pub async fn refresh_mcp_tools(&self) -> anyhow::Result<()> {
        let mcp_tools = loop_agent::harness::mcp::bridge::mcp_tools_to_agent_tools_async(
            self.mcp_client.connections(),
        )
        .await;
        let mut all_tools: Vec<_> = self
            .harness
            .get_tools()
            .await
            .into_iter()
            .filter(|tool| !tool.name.starts_with("mcp__"))
            .collect();
        all_tools.extend(mcp_tools);
        self.harness
            .set_tools(all_tools)
            .await
            .map_err(|e| anyhow::anyhow!("set MCP tools: {e}"))
    }

    /// `/login`: save the key (and custom entry), register the provider and list its
    /// models. Nothing is kept if the key is rejected or the listing fails.
    pub async fn connect_provider(
        &mut self,
        request: &crate::config::ProviderLoginRequest,
    ) -> anyhow::Result<ConnectedProvider> {
        use crate::config::providers::{replace_api_key, restore_api_key, upsert_custom_provider};

        request.validate()?;
        let (id, name) = (request.provider_id(), request.display_name());
        if let (Some(preset), Some(key)) = (request.preset(), request.api_key()) {
            preset
                .verify_key(key)
                .await
                .map_err(|e| anyhow::anyhow!("{name} rejected the key: {e}"))?;
        }
        let previous_key = replace_api_key(self.credentials.as_ref(), &id, request.api_key());
        let entry = request.custom_entry();
        let previous_provider = self.models.get_provider(&id);
        if let Some(entry) = &entry {
            self.models.set_provider(entry.provider());
        }

        let refresh = self
            .models
            .refresh(ModelsRefreshOptions {
                allow_network: Some(true),
                force: true,
                provider_id: Some(id.clone()),
            })
            .await;
        let model_count = self.models.get_models(Some(&id)).len();
        let failure = refresh
            .errors
            .get(&id)
            .cloned()
            .or_else(|| (model_count == 0).then(|| "no models were listed".to_string()));
        if let Some(err) = failure {
            restore_api_key(self.credentials.as_ref(), &id, previous_key);
            if entry.is_some() {
                match previous_provider {
                    Some(provider) => self.models.set_provider(provider),
                    None => {
                        self.models.remove_provider(&id);
                    }
                }
            }
            anyhow::bail!("could not list {name} models: {err}");
        }

        if let Some(entry) = entry {
            upsert_custom_provider(&mut self.settings.providers, entry);
            self.save_settings()?;
        }
        self.needs_provider_setup = false;
        Ok(ConnectedProvider {
            id,
            name,
            model_count,
        })
    }

    /// `/logout`: forget the provider's key; custom providers are removed entirely. A
    /// selected model from that provider is deselected.
    pub async fn disconnect_provider(&mut self, id: &str) -> anyhow::Result<String> {
        let id = id.trim().to_ascii_lowercase();
        let custom_index = self.settings.providers.iter().position(|p| p.id == id);
        let preset = loop_ai::providers::provider_preset(&id);
        let had_key = self.credentials.get(&id).is_some();
        if custom_index.is_none() && !had_key {
            anyhow::bail!("{id} is not connected");
        }
        self.credentials.remove(&id);
        let name = match custom_index {
            Some(index) => {
                let entry = self.settings.providers.remove(index);
                self.models.remove_provider(&id);
                self.save_settings()?;
                entry.name
            }
            None => preset.map_or_else(|| id.clone(), |p| p.name.to_string()),
        };
        if self
            .selected_model
            .as_ref()
            .is_some_and(|m| m.provider == id)
        {
            self.model_note = self
                .selected_model_spec()
                .map(|spec| format!("{spec} was disconnected"));
            self.selected_model = None;
            self.harness.clear_model().await;
            self.settings.clear_selected_model();
            self.save_settings()?;
        }
        Ok(name)
    }

    /// Select `provider/id` for this and future runs (harness + saved settings).
    pub async fn select_model(
        &mut self,
        provider: &str,
        id: &str,
    ) -> anyhow::Result<loop_ai::Model> {
        let model = self
            .models
            .get_model(provider, id)
            .ok_or_else(|| anyhow::anyhow!("model not found: {provider}/{id}"))?;
        self.harness.set_model(model.clone()).await;
        self.settings.set_selected_model(provider, id);
        self.selected_model = Some(model.clone());
        self.model_note = None;
        self.save_settings()?;
        Ok(model)
    }

    /// `provider/id` of the selected model.
    pub fn selected_model_spec(&self) -> Option<String> {
        self.selected_model
            .as_ref()
            .map(|m| format!("{}/{}", m.provider, m.id))
    }

    /// Models of connected providers, in picker order (Soket first).
    pub async fn available_models(&self) -> Vec<loop_ai::Model> {
        crate::config::providers::sort_models_for_picker(self.models.get_available().await)
    }

    /// Ids of providers usable right now (presets with a key, then custom providers).
    pub fn connected_providers(&self) -> Vec<String> {
        crate::config::providers::connected_providers(
            self.credentials.as_ref(),
            &self.settings.providers,
            |k| std::env::var(k).ok(),
        )
    }
}

#[cfg(feature = "telemetry")]
impl Runtime {
    /// Take ownership of the process telemetry and apply saved tracing settings.
    pub fn attach_telemetry(
        &mut self,
        handle: loop_telemetry::TelemetryHandle,
    ) -> anyhow::Result<loop_telemetry::TelemetryStatus> {
        self.telemetry = Some(handle);
        self.tracing_control()?.apply()
    }

    /// Current tracing state, if telemetry is attached.
    pub fn tracing_status(&self) -> Option<loop_telemetry::TelemetryStatus> {
        self.telemetry.as_ref().map(|t| t.status())
    }

    /// `/tracing enable|disable`, persisted to global settings.
    pub fn set_tracing_enabled(
        &mut self,
        enabled: bool,
    ) -> anyhow::Result<loop_telemetry::TelemetryStatus> {
        let status = self.tracing_control()?.set_enabled(enabled);
        self.save_settings()?;
        Ok(status)
    }

    /// `/tracing setup`, persisted to global settings and the credential store.
    pub fn setup_tracing(
        &mut self,
        request: &crate::config::TracingSetupRequest,
    ) -> anyhow::Result<loop_telemetry::TelemetryStatus> {
        let status = self.tracing_control()?.setup(request)?;
        self.save_settings()?;
        Ok(status)
    }

    fn tracing_control(&mut self) -> anyhow::Result<crate::config::TracingControl<'_>> {
        let handle = self
            .telemetry
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("tracing is not available in this mode"))?;
        Ok(crate::config::TracingControl {
            settings: &mut self.settings.tracing,
            store: self.credentials.as_ref(),
            handle,
        })
    }
}

/// Result of a successful `/login`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectedProvider {
    /// Provider id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Models listed after connecting.
    pub model_count: usize,
}

/// CLI bootstrap flags affecting runtime.
pub struct BootstrapOpts {
    /// Working directory.
    pub cwd: PathBuf,
    /// Provider override.
    pub provider: Option<String>,
    /// Model override.
    pub model: Option<String>,
    /// Theme override.
    pub theme: Option<String>,
    /// System prompt override.
    pub system_prompt: Option<String>,
    /// Append system prompt.
    pub append_system_prompt: Option<String>,
    /// Skip context files.
    pub no_context_files: bool,
    /// Interactive (prompt for API key).
    pub interactive: bool,
    /// Resume session id.
    pub session_id: Option<String>,
    /// Start with this external agent. Loop's own provider is then optional.
    pub agent: Option<String>,
}

/// Build the standard tool set bound to an execution environment.
pub fn build_tools(env: Arc<dyn loop_agent::harness::ExecutionEnv>) -> Vec<AgentTool> {
    vec![
        create_read_tool(Arc::clone(&env)),
        create_write_tool(Arc::clone(&env)),
        create_edit_tool(Arc::clone(&env)),
        create_bash_tool(env),
    ]
}

/// Ensure at least one LLM provider is connected, or defer to the TUI's `/login` wizard.
///
/// Returns `true` when the TUI should open the first-run provider setup.
pub fn ensure_provider_connected(
    store: &FileCredentialStore,
    custom: &[crate::config::CustomProviderEntry],
    interactive: bool,
) -> anyhow::Result<bool> {
    let connected =
        crate::config::providers::connected_providers(store, custom, |k| std::env::var(k).ok());
    if !connected.is_empty() {
        return Ok(false);
    }
    if !interactive {
        bail!(
            "No LLM provider connected. Set SOKET_API_KEY, OPENROUTER_API_KEY or OPENAI_API_KEY, \
             or run `loop` and use /login."
        );
    }
    Ok(true)
}

/// Build models with every preset, saved custom providers and `models.json` customs.
pub fn build_models(
    agent_dir: &Path,
    credentials: Arc<FileCredentialStore>,
    custom: &[crate::config::CustomProviderEntry],
) -> anyhow::Result<Arc<Models>> {
    let store = Arc::new(FileModelsStore::new(models_store_path(agent_dir)));
    let models = Arc::new(Models::create(CreateModelsOptions {
        credentials: Some(credentials),
        models_store: Some(store),
    }));
    crate::config::providers::register_providers(&models, custom);
    load_custom_models_json(agent_dir, &models)?;
    Ok(models)
}

fn load_custom_models_json(agent_dir: &Path, models: &Models) -> anyhow::Result<()> {
    let path = models_json_path(agent_dir);
    if !path.exists() {
        return Ok(());
    }
    let raw = std::fs::read_to_string(path)?;
    let value: serde_json::Value = serde_json::from_str(&raw)?;
    // Support { "providers": [ { id, baseUrl, apiKeyEnv, models: ["id"] } ] }
    if let Some(arr) = value.get("providers").and_then(|v| v.as_array()) {
        for p in arr {
            let id = p.get("id").and_then(|v| v.as_str()).unwrap_or("custom");
            let base_url = p
                .get("baseUrl")
                .or_else(|| p.get("base_url"))
                .and_then(|v| v.as_str())
                .unwrap_or("http://localhost:11434/v1");
            let api_key_env = p
                .get("apiKeyEnv")
                .or_else(|| p.get("api_key_env"))
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(|s| s.to_string()))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let model_specs = p
                .get("models")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|m| {
                            if let Some(s) = m.as_str() {
                                Some(CustomModelSpec::new(s))
                            } else {
                                let id = m.get("id")?.as_str()?;
                                Some(CustomModelSpec::new(id))
                            }
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            if model_specs.is_empty() {
                continue;
            }
            models.set_provider(custom_provider(CustomProviderConfig {
                id: id.into(),
                name: p
                    .get("name")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                base_url: base_url.into(),
                api_key_env,
                models: model_specs,
                headers: None,
            }));
        }
    }
    Ok(())
}

/// Resolve project trust (ask interactively when needed).
pub fn resolve_trust(
    trust: &mut TrustStore,
    cwd: &Path,
    settings: &Settings,
    interactive: bool,
) -> anyhow::Result<bool> {
    if let Some(v) = trust.get(cwd) {
        return Ok(v);
    }
    match settings.default_project_trust.as_str() {
        "always" => {
            trust.set(cwd, true)?;
            Ok(true)
        }
        "never" => {
            trust.set(cwd, false)?;
            Ok(false)
        }
        _ => {
            if !interactive {
                return Ok(false);
            }
            eprintln!("Trust project config from {}? [y/N]", cwd.display());
            let mut answer = String::new();
            std::io::Write::write_all(&mut std::io::stderr(), b"> ").ok();
            std::io::stdin().read_line(&mut answer)?;
            let yes = matches!(answer.trim().to_lowercase().as_str(), "y" | "yes");
            trust.set(cwd, yes)?;
            Ok(yes)
        }
    }
}

/// Reasoning effort levels of Loop's own agent, as `/effort` names them.
const NATIVE_EFFORTS: [&str; 7] = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

/// The levels of `candidates` that `model` accepts: only `off` for a model
/// without reasoning, and none its thinking map marks unsupported.
fn native_efforts(model: Option<&loop_ai::Model>, candidates: &[&str]) -> Vec<String> {
    candidates
        .iter()
        .filter(|level| {
            let Some(model) = model else {
                return true;
            };
            let Some(reasoning) = parse_thinking(level).to_reasoning() else {
                return true; // "off" is always available
            };
            model.reasoning
                && !loop_ai::reasoning_rejected(model)
                && model.thinking_level_map.as_ref().is_none_or(|map| {
                    map.get(&loop_ai::ModelThinkingLevel::from(reasoning)) != Some(&None)
                })
        })
        .map(|level| (*level).to_owned())
        .collect()
}

fn parse_thinking(s: &str) -> AgentThinkingLevel {
    match s.to_lowercase().as_str() {
        "minimal" => AgentThinkingLevel::Minimal,
        "low" => AgentThinkingLevel::Low,
        "medium" => AgentThinkingLevel::Medium,
        "high" => AgentThinkingLevel::High,
        "xhigh" => AgentThinkingLevel::XHigh,
        "max" => AgentThinkingLevel::Max,
        _ => AgentThinkingLevel::Off,
    }
}

fn thinking_label(level: AgentThinkingLevel) -> &'static str {
    match level {
        AgentThinkingLevel::Off => "off",
        AgentThinkingLevel::Minimal => "minimal",
        AgentThinkingLevel::Low => "low",
        AgentThinkingLevel::Medium => "medium",
        AgentThinkingLevel::High => "high",
        AgentThinkingLevel::XHigh => "xhigh",
        AgentThinkingLevel::Max => "max",
    }
}

/// Bootstrap the full runtime.
pub async fn bootstrap(opts: BootstrapOpts) -> anyhow::Result<Runtime> {
    let agent_dir = get_agent_dir();
    ensure_agent_dirs(&agent_dir)?;
    std::env::set_var("LOOP_CODING_AGENT", "true");

    let mut trust = TrustStore::load(trust_path(&agent_dir))?;
    // Load global settings first for trust default
    let global_settings = Settings::load_file(&settings_path(&agent_dir))?;
    if !settings_path(&agent_dir).exists() {
        global_settings.save_file(&settings_path(&agent_dir))?;
    }

    let project_trusted = resolve_trust(&mut trust, &opts.cwd, &global_settings, opts.interactive)?;
    let mut settings = load_settings(&agent_dir, &opts.cwd, project_trusted)?;
    if let Some(t) = &opts.theme {
        settings.theme = t.clone();
    }

    let credentials = Arc::new(FileCredentialStore::open(auth_path(&agent_dir))?);
    let agents = Arc::new(crate::agents::AgentManager::new(
        crate::agents::agent_entries(&settings.agents),
        opts.cwd.clone(),
        &agent_dir,
    ));
    // A signed-in subscription is a connected provider too.
    let subscribed = agents
        .entries()
        .iter()
        .any(|e| e.needs_sign_in() && agents.is_signed_in(&e.id));
    let needs_provider_setup = opts.agent.is_none()
        && !subscribed
        && ensure_provider_connected(&credentials, &settings.providers, opts.interactive)?;

    let models = build_models(&agent_dir, Arc::clone(&credentials), &settings.providers)?;
    // Hydrate every provider from models-store.json before resolving a model. Without
    // this, `--print` races the background `/v1/models` refresh.
    let _ = models
        .refresh(ModelsRefreshOptions {
            allow_network: Some(false),
            force: false,
            provider_id: None,
        })
        .await;
    if !needs_provider_setup {
        // Network catalog refresh in the background so interactive startup
        // isn't blocked by a slow API. Cached models are already in memory.
        let bg_models = Arc::clone(&models);
        tokio::spawn(async move {
            let refresh = bg_models
                .refresh(ModelsRefreshOptions {
                    allow_network: Some(true),
                    force: true,
                    // Providers without a key stay on their cache (see the fetcher).
                    provider_id: None,
                })
                .await;
            for (pid, err) in &refresh.errors {
                tracing::warn!("model refresh {pid}: {err}");
            }
        });
    }

    // A saved subscription model is started by the surface, not Loop's agent.
    let saved = settings
        .selected_model()
        .filter(|(provider, _)| agents.entry(provider).is_none());
    let (mut model, mut model_note) = match crate::model_selection::resolve_startup_model(
        &models,
        saved,
        opts.provider.as_deref(),
        opts.model.as_deref(),
    )? {
        StartupModel::Selected(model) => (Some(*model), None),
        StartupModel::NotSelected { reason } => (None, reason),
    };
    if let Some(m) = &model {
        settings.set_selected_model(&m.provider, &m.id);
    }

    let resources = load_resources(&agent_dir, &opts.cwd, project_trusted, &settings);
    let context_files = if opts.no_context_files {
        vec![]
    } else {
        load_context_files(&opts.cwd, &agent_dir)
    };
    let (custom, append) = resolve_system_prompt_files(
        &opts.cwd,
        project_trusted,
        opts.system_prompt.as_deref(),
        opts.append_system_prompt.as_deref(),
    );
    let snippets = default_tool_snippets();
    let selected = ["read", "bash", "edit", "write"];
    let system_prompt = build_system_prompt(BuildSystemPromptOptions {
        custom_prompt: custom.as_deref(),
        append_system_prompt: append.as_deref(),
        cwd: &opts.cwd,
        selected_tools: &selected,
        tool_snippets: &snippets,
        context_files: &context_files,
    });

    let sessions_db = sessions_db_path(&agent_dir);
    let store = create_sqlite_session_store(&sessions_db)
        .map_err(|e| anyhow::anyhow!("sqlite session store: {e}"))?;
    let repo = create_session_repository(store, None);
    let resumed = opts.session_id.is_some();
    let session = if let Some(id) = &opts.session_id {
        repo.open(id)
            .await
            .map_err(|e| anyhow::anyhow!("load session: {e}"))?
    } else {
        repo.create(Some(opts.cwd.to_string_lossy().into_owned()), None)
            .await
            .map_err(|e| anyhow::anyhow!("create session: {e}"))?
    };

    // Restore model from the session branch when resuming (unless CLI overrides).
    if resumed {
        if let Ok(ctx) = session.build_context().await {
            if opts.provider.is_none() && opts.model.is_none() {
                if let Some((p, m)) = &ctx.model {
                    if let Some(resolved) = models.get_model(p, m) {
                        settings.set_selected_model(p, m);
                        model = Some(resolved);
                        model_note = None;
                    }
                }
            }
            // Thinking-level changes are recorded on the branch when present.
            if !matches!(ctx.thinking_level, AgentThinkingLevel::Off) {
                settings.default_thinking_level = thinking_label(ctx.thinking_level).into();
            }
        }
    }

    let host: Arc<dyn loop_agent::harness::ExecutionEnv> =
        Arc::new(HostExecutionEnv::new(&opts.cwd));

    let (sandbox, tool_env): (SandboxMode, Arc<dyn loop_agent::harness::ExecutionEnv>) =
        match settings.sandbox.mode.as_str() {
            "local" => {
                let isolation = KrunIsolation::parse(&settings.sandbox.isolation)
                    .unwrap_or(KrunIsolation::Full);
                let oci_runtime = LocalSandboxRuntime::parse(&settings.sandbox.runtime)
                    .unwrap_or(LocalSandboxRuntime::Runc);
                settings.sandbox.isolation = isolation.as_str().into();
                settings.sandbox.runtime = oci_runtime.as_str().into();
                let sb = KrunSandbox::new(KrunSandbox::config_for(
                    opts.cwd.clone(),
                    isolation,
                    oci_runtime,
                ));
                match sb.start().await {
                    Ok(()) => {
                        let env = sb.env();
                        (
                            SandboxMode::Enabled {
                                sandbox: Arc::new(sb),
                            },
                            env,
                        )
                    }
                    Err(e) => {
                        tracing::warn!("local sandbox disabled at startup: {e}");
                        eprintln!("warning: local sandbox not enabled:\n{e}");
                        settings.sandbox.mode = "off".into();
                        (SandboxMode::Disabled, Arc::clone(&host))
                    }
                }
            }
            _ => {
                settings.sandbox.mode = "off".into();
                (SandboxMode::Disabled, Arc::clone(&host))
            }
        };

    let tools = build_tools(Arc::clone(&tool_env));

    let session_id = session.metadata().id.clone();
    let harness = Arc::new(AgentHarness::new(AgentHarnessOptions {
        models: Arc::clone(&models),
        model: model.clone(),
        session,
        host_env: host,
        tools,
        system_prompt,
        sandbox,
        resources: AgentHarnessResources {
            skills: resources.skills.clone(),
            prompt_templates: resources.prompts.clone(),
        },
    }));
    harness
        .set_thinking_level(parse_thinking(&settings.default_thinking_level))
        .await;
    harness
        .set_response_header_timeout_ms(settings.response_header_timeout_ms)
        .await;

    let native = loop_agent::native_backend::NativeBackend::new(Arc::clone(&harness)).await;
    let backend: Arc<dyn loop_protocol::AgentBackend> = native.clone();

    crate::hooks_load::register_json_hooks(&harness, &resources.hook_paths);
    let ext = crate::extensions::load_extensions(&resources.extension_paths);
    for n in &ext.notices {
        tracing::info!("extension: {n}");
    }

    let theme_dirs = theme_search_dirs(
        &agent_dir,
        project_trusted
            .then_some(crate::config::paths::get_project_dir(&opts.cwd))
            .as_deref(),
    );
    let theme = Theme::load(&settings.theme, &theme_dirs).unwrap_or_else(|_| Theme::dark());

    let mcp_client = Arc::new(loop_mcp::McpClientManager::with_oauth_dir(
        agent_dir.join("mcp-auth"),
    ));
    let entries = enabled_mcp_server_entries(&settings.mcp_servers, &settings.disabled_mcp_servers);
    if !entries.is_empty() {
        let results = mcp_client.connect_all(&entries).await;
        for (name, result) in &results {
            match result {
                Ok(count) => tracing::info!("mcp: connected to '{name}' ({count} tools)"),
                Err(e) => tracing::warn!("mcp: failed to connect to '{name}': {e}"),
            }
        }
        let mcp_tools = loop_agent::harness::mcp::bridge::mcp_tools_to_agent_tools_async(
            mcp_client.connections(),
        )
        .await;
        if !mcp_tools.is_empty() {
            let mut all_tools = harness.get_tools().await;
            all_tools.extend(mcp_tools);
            harness
                .set_tools(all_tools)
                .await
                .map_err(|e| anyhow::anyhow!("set MCP tools: {e}"))?;
        }
    }

    Ok(Runtime {
        agent_dir,
        cwd: opts.cwd,
        settings,
        models,
        credentials,
        harness,
        backend,
        native,
        agents,
        active_agent: None,
        agent_model: None,
        agent_effort: None,
        theme,
        resources,
        project_trusted,
        trust,
        sessions_db,
        session_id,
        resumed,
        needs_provider_setup,
        selected_model: model,
        model_note,
        tool_approval: None,
        mcp_client,
        active_skills: Vec::new(),
        #[cfg(feature = "telemetry")]
        telemetry: None,
    })
}

/// Result of enabling or disabling one configured MCP server.
#[derive(Debug)]
pub enum McpToggleOutcome {
    /// No server with that name is configured.
    Missing,
    /// The enabled state was saved and the connection matches it.
    Updated,
    /// The server was marked enabled, but connecting to it failed.
    ConnectFailed(String),
}

/// MCP entries that are configured and not listed in `disabled`.
pub fn enabled_mcp_server_entries(
    configs: &std::collections::BTreeMap<String, McpServerConfig>,
    disabled: &[String],
) -> Vec<loop_mcp::McpServerEntry> {
    mcp_server_entries(configs)
        .into_iter()
        .filter(|entry| !disabled.iter().any(|name| name == &entry.name))
        .collect()
}

/// Convert settings MCP config into client entries.
pub fn mcp_server_entries(
    configs: &std::collections::BTreeMap<String, McpServerConfig>,
) -> Vec<loop_mcp::McpServerEntry> {
    let mut entries = Vec::new();
    for (name, cfg) in configs {
        let transport = if let Some(url) = &cfg.url {
            loop_mcp::McpTransport::Http {
                url: url.clone(),
                headers: cfg
                    .headers
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
            }
        } else if let Some(command) = &cfg.command {
            loop_mcp::McpTransport::Stdio {
                command: command.clone(),
                args: cfg.args.clone(),
                env: cfg
                    .env
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
            }
        } else {
            tracing::warn!("mcp: skipping '{name}': neither 'command' nor 'url' configured");
            continue;
        };
        entries.push(loop_mcp::McpServerEntry {
            name: name.clone(),
            transport,
        });
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_efforts_follow_the_model() {
        let all = ["off", "low", "high"];
        let mut model = loop_ai::providers::soket_seed_models().remove(0);
        assert_eq!(native_efforts(Some(&model), &all), ["off", "low", "high"]);

        model.thinking_level_map = Some(std::collections::HashMap::from([(
            loop_ai::ModelThinkingLevel::High,
            None,
        )]));
        assert_eq!(native_efforts(Some(&model), &all), ["off", "low"]);

        model.reasoning = false;
        assert_eq!(native_efforts(Some(&model), &all), ["off"]);
        assert_eq!(native_efforts(None, &all), ["off", "low", "high"]);
    }
}
