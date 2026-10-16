//! Global and project settings.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::paths::{get_project_dir, settings_path};
use super::providers::CustomProviderEntry;
use super::tracing::TracingSettings;

/// Compaction settings subset.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionSettingsJson {
    /// Enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Reserve tokens.
    #[serde(default = "default_reserve")]
    pub reserve_tokens: u64,
    /// Keep recent tokens.
    #[serde(default = "default_keep")]
    pub keep_recent_tokens: u64,
}

fn default_true() -> bool {
    true
}
fn default_reserve() -> u64 {
    16_384
}
fn default_keep() -> u64 {
    20_000
}

impl Default for CompactionSettingsJson {
    fn default() -> Self {
        Self {
            enabled: true,
            reserve_tokens: default_reserve(),
            keep_recent_tokens: default_keep(),
        }
    }
}

/// Sandbox settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxSettings {
    /// `off` or `local`.
    #[serde(default = "default_sandbox_mode")]
    pub mode: String,
    /// When `mode` is `local`: `full` or `partial`.
    #[serde(default = "default_sandbox_isolation")]
    pub isolation: String,
    /// When `mode` is `local`: `runc` (default), `crun`, `runsc`, or `krun`.
    #[serde(default = "default_sandbox_runtime")]
    pub runtime: String,
}

fn default_sandbox_mode() -> String {
    "off".into()
}

/// Configuration for an external MCP server (stdio or remote HTTP).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerConfig {
    /// Command to spawn a stdio MCP server. Mutually exclusive with `url`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Arguments passed to the command (stdio only).
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment variables for the child process (stdio only).
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// URL for a remote streamable-HTTP MCP server. Mutually exclusive with `command`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Extra HTTP headers sent with every request (remote only).
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

fn default_sandbox_isolation() -> String {
    "full".into()
}

fn default_sandbox_runtime() -> String {
    "runc".into()
}

impl Default for SandboxSettings {
    fn default() -> Self {
        Self {
            mode: default_sandbox_mode(),
            isolation: default_sandbox_isolation(),
            runtime: default_sandbox_runtime(),
        }
    }
}

impl SandboxSettings {
    /// Human-readable short status (settings only; prefer harness `sandbox_info` for live details).
    pub fn display(&self) -> String {
        match self.mode.as_str() {
            "local" => format!("local --{} --{}", self.isolation, self.runtime),
            other => other.to_string(),
        }
    }
}

/// User / project settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    /// Default provider id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_provider: Option<String>,
    /// Selected model id; `None` until the user picks one with `/model`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    /// Default thinking level (`off`, `low`, …).
    #[serde(default = "default_thinking")]
    pub default_thinking_level: String,
    /// Theme name.
    #[serde(default = "default_theme")]
    pub theme: String,
    /// Compaction.
    #[serde(default)]
    pub compaction: CompactionSettingsJson,
    /// Models enabled for Ctrl+P cycling (globs).
    #[serde(default)]
    pub enabled_models: Vec<String>,
    /// Extra skill paths / globs.
    #[serde(default)]
    pub skills: Vec<String>,
    /// Skill names hidden from the model and from `/skill:` commands.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disabled_skills: Vec<String>,
    /// Extra extension paths.
    #[serde(default)]
    pub extensions: Vec<String>,
    /// Extra prompt paths.
    #[serde(default)]
    pub prompts: Vec<String>,
    /// Extra theme paths.
    #[serde(default)]
    pub themes: Vec<String>,
    /// Enable `/skill:name` commands.
    #[serde(default = "default_true")]
    pub enable_skill_commands: bool,
    /// Quiet startup banner.
    #[serde(default)]
    pub quiet_startup: bool,
    /// Hide thinking blocks in UI.
    #[serde(default)]
    pub hide_thinking_block: bool,
    /// Double-escape action: `tree`, `fork`, `none`.
    #[serde(default = "default_double_escape")]
    pub double_escape_action: String,
    /// UI mode: `normal` or `fullscreen`.
    #[serde(default = "default_ui_mode")]
    pub ui_mode: String,
    /// Sandbox.
    #[serde(default)]
    pub sandbox: SandboxSettings,
    /// `web_fetch` tool (`webFetch` in settings.json).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_fetch: Option<loop_agent::harness::WebFetchConfig>,
    /// Web tools switch and `web_search` backend (`webSearch` in
    /// settings.json). Absent means on, with the Soket backend; the search
    /// tool is only registered when the backend's API key resolves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_search: Option<loop_agent::harness::WebSearchConfig>,
    /// When to prompt for `ask`-mode tools: `newSession`, `always`, or `never`.
    #[serde(default = "default_file_edit_review")]
    pub file_edit_review: String,
    /// Diff editor binary (`cursor`, `code`, or absolute path). Auto-detected when empty.
    #[serde(default)]
    pub diff_editor: Option<String>,
    /// Per-tool defaults: `ask` | `allow` | `deny` (stored in `~/.loop/agent/settings.json`).
    /// Interactive workflows currently exist for `write`, `edit`, and `bash`.
    #[serde(default = "default_tool_permissions_settings")]
    pub tool_permissions: BTreeMap<String, String>,
    /// Default project trust: `ask`, `always`, `never` (global only).
    #[serde(default = "default_trust")]
    pub default_project_trust: String,
    /// External MCP servers keyed by name.
    #[serde(default)]
    pub mcp_servers: BTreeMap<String, McpServerConfig>,
    /// Configured MCP server names that are not connected.
    ///
    /// Saved in the global `settings.json` as `disabledMcpServers`. A project
    /// settings file can add more names; they are combined with the global list.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disabled_mcp_servers: Vec<String>,
    /// Max wait for the model HTTP response headers (time-to-first-byte), in ms.
    ///
    /// Default `60000`. Set to `0` to wait indefinitely (useful for long-running
    /// workflows / slow models). Esc still aborts the turn.
    #[serde(default = "default_response_header_timeout_ms")]
    pub response_header_timeout_ms: u64,
    /// Langfuse tracing (global only; see [`TracingSettings`]).
    #[serde(default)]
    pub tracing: TracingSettings,
    /// Custom OpenAI-compatible providers added with `/login` (global only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<CustomProviderEntry>,
    /// External ACP agents (global only: a project file must not add
    /// commands Loop runs). Entries replace built-in presets with the same id.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agents: Vec<crate::agents::AgentEntry>,
}

fn default_thinking() -> String {
    "off".into()
}
fn default_theme() -> String {
    "dark".into()
}
fn default_double_escape() -> String {
    "tree".into()
}
fn default_ui_mode() -> String {
    "normal".into()
}
fn default_trust() -> String {
    "ask".into()
}
fn default_file_edit_review() -> String {
    "newSession".into()
}
fn default_tool_permissions_settings() -> BTreeMap<String, String> {
    crate::tool_approval::default_tool_permissions()
}
fn default_response_header_timeout_ms() -> u64 {
    60_000
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            default_provider: None,
            default_model: None,
            default_thinking_level: default_thinking(),
            theme: default_theme(),
            compaction: CompactionSettingsJson::default(),
            enabled_models: Vec::new(),
            skills: Vec::new(),
            disabled_skills: Vec::new(),
            extensions: Vec::new(),
            prompts: Vec::new(),
            themes: Vec::new(),
            enable_skill_commands: true,
            quiet_startup: false,
            hide_thinking_block: false,
            double_escape_action: default_double_escape(),
            ui_mode: default_ui_mode(),
            sandbox: SandboxSettings::default(),
            web_fetch: None,
            web_search: None,
            file_edit_review: default_file_edit_review(),
            diff_editor: None,
            tool_permissions: default_tool_permissions_settings(),
            default_project_trust: default_trust(),
            mcp_servers: BTreeMap::new(),
            disabled_mcp_servers: Vec::new(),
            response_header_timeout_ms: default_response_header_timeout_ms(),
            tracing: TracingSettings::default(),
            providers: Vec::new(),
            agents: Vec::new(),
        }
    }
}

impl Settings {
    /// The saved `(provider, model id)`, when both are set.
    pub fn selected_model(&self) -> Option<(&str, &str)> {
        Some((
            self.default_provider.as_deref()?,
            self.default_model.as_deref()?,
        ))
    }

    /// `provider/model id` of the saved selection.
    pub fn selected_model_spec(&self) -> Option<String> {
        self.selected_model().map(|(p, m)| format!("{p}/{m}"))
    }

    /// Remember a model selection.
    pub fn set_selected_model(&mut self, provider: &str, model: &str) {
        self.default_provider = Some(provider.into());
        self.default_model = Some(model.into());
    }

    /// Forget the model selection.
    pub fn clear_selected_model(&mut self) {
        self.default_provider = None;
        self.default_model = None;
    }

    /// Load from a JSON file, or defaults if missing.
    pub fn load_file(path: &Path) -> anyhow::Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(path)?;
        if raw.trim().is_empty() {
            return Ok(Self::default());
        }
        Ok(serde_json::from_str(&raw)?)
    }

    /// Save to disk.
    pub fn save_file(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(path, json)?;
        Ok(())
    }

    /// Deep-merge project settings over global (arrays replace).
    pub fn merge_project(&mut self, project: Settings) {
        // Scalars / objects from project always override when present in file —
        // we treat the deserialized project file as authoritative for set fields
        // by replacing known top-level keys that differ from defaults only if
        // the project file existed. Callers pass fully loaded project Settings.
        *self = project_overlay(self.clone(), project);
    }
}

fn project_overlay(mut base: Settings, project: Settings) -> Settings {
    // Prefer project values when the project settings file was loaded (non-default merge).
    if let (Some(provider), Some(model)) = (project.default_provider, project.default_model) {
        base.default_provider = Some(provider);
        base.default_model = Some(model);
    }
    base.default_thinking_level = project.default_thinking_level;
    base.theme = project.theme;
    base.compaction = project.compaction;
    if !project.enabled_models.is_empty() {
        base.enabled_models = project.enabled_models;
    }
    if !project.skills.is_empty() {
        base.skills = project.skills;
    }
    for name in project.disabled_skills {
        if !base.disabled_skills.contains(&name) {
            base.disabled_skills.push(name);
        }
    }
    if !project.extensions.is_empty() {
        base.extensions = project.extensions;
    }
    if !project.prompts.is_empty() {
        base.prompts = project.prompts;
    }
    if !project.themes.is_empty() {
        base.themes = project.themes;
    }
    base.enable_skill_commands = project.enable_skill_commands;
    base.quiet_startup = project.quiet_startup;
    base.hide_thinking_block = project.hide_thinking_block;
    base.double_escape_action = project.double_escape_action;
    base.ui_mode = project.ui_mode;
    base.sandbox = project.sandbox;
    if let Some(project_fetch) = project.web_fetch {
        let global = base.web_fetch.unwrap_or_default();
        base.web_fetch = Some(loop_agent::harness::WebFetchConfig {
            allow_private_addresses: global.allow_private_addresses,
            provider: project_fetch.provider,
        });
    }
    if let Some(project_search) = project.web_search {
        // A project may pick the backend or turn the web tools off, but not
        // change where keys are sent or which env vars are read: `baseUrl`,
        // `apiKeyEnv` and `allowPrivateAddresses` stay global-only.
        let global = base.web_search.take().unwrap_or_default();
        base.web_search = Some(loop_agent::harness::WebSearchConfig {
            enabled: global.enabled && project_search.enabled,
            provider: project_search.provider.or(global.provider),
            ..global
        });
    }
    base.file_edit_review = project.file_edit_review;
    if project.diff_editor.is_some() {
        base.diff_editor = project.diff_editor;
    }
    if !project.tool_permissions.is_empty() {
        for (k, v) in project.tool_permissions {
            base.tool_permissions.insert(k, v);
        }
    }
    if !project.mcp_servers.is_empty() {
        for (k, v) in project.mcp_servers {
            base.mcp_servers.insert(k, v);
        }
    }
    for name in project.disabled_mcp_servers {
        if !base.disabled_mcp_servers.contains(&name) {
            base.disabled_mcp_servers.push(name);
        }
    }
    base.response_header_timeout_ms = project.response_header_timeout_ms;
    base
}

/// Load global settings then overlay trusted project settings.
pub fn load_settings(
    agent_dir: &Path,
    cwd: &Path,
    project_trusted: bool,
) -> anyhow::Result<Settings> {
    let mut settings = Settings::load_file(&settings_path(agent_dir))?;
    crate::tool_approval::merge_default_tool_permissions(&mut settings.tool_permissions);
    if project_trusted {
        let project_settings = get_project_dir(cwd).join("settings.json");
        if project_settings.exists() {
            let project = Settings::load_file(&project_settings)?;
            settings.merge_project(project);
        }
    }
    Ok(settings)
}

/// Add (`enabled = false`) or remove (`enabled = true`) `name` in a disabled-name list.
pub fn set_disabled_name(list: &mut Vec<String>, name: &str, enabled: bool) {
    if enabled {
        list.retain(|n| n != name);
    } else if !list.iter().any(|n| n == name) {
        list.push(name.to_string());
    }
}

/// Add (`enabled = false`) or remove (`enabled = true`) `name` in a disabled-skills list.
pub fn set_skill_disabled(list: &mut Vec<String>, name: &str, enabled: bool) {
    set_disabled_name(list, name, enabled);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_mcp_servers_round_trip_and_merge() {
        let mut settings = Settings::default();
        assert!(!serde_json::to_string(&settings)
            .unwrap()
            .contains("disabledMcpServers"));
        set_disabled_name(&mut settings.disabled_mcp_servers, "filesystem", false);
        set_disabled_name(&mut settings.disabled_mcp_servers, "filesystem", false);
        assert_eq!(settings.disabled_mcp_servers, ["filesystem"]);
        let raw = serde_json::to_string(&settings).unwrap();
        assert!(raw.contains("\"disabledMcpServers\":[\"filesystem\"]"));
        set_disabled_name(&mut settings.disabled_mcp_servers, "filesystem", true);
        assert!(settings.disabled_mcp_servers.is_empty());

        let mut global = Settings {
            disabled_mcp_servers: vec!["filesystem".into()],
            ..Default::default()
        };
        let project = Settings {
            disabled_mcp_servers: vec!["filesystem".into(), "remote".into()],
            ..Default::default()
        };
        global.merge_project(project);
        assert_eq!(global.disabled_mcp_servers, ["filesystem", "remote"]);
    }

    #[test]
    fn project_web_search_cannot_redirect_keys() {
        use loop_agent::harness::WebSearchConfig;
        let mut global = Settings {
            web_search: Some(WebSearchConfig {
                provider: Some("tavily".into()),
                api_key_env: vec!["MY_TAVILY_KEY".into()],
                ..Default::default()
            }),
            ..Default::default()
        };
        let project: Settings = serde_json::from_str(
            r#"{"webSearch": {"provider": "brave", "baseUrl": "https://attacker.example",
                "apiKeyEnv": ["AWS_SECRET_ACCESS_KEY"], "allowPrivateAddresses": true}}"#,
        )
        .unwrap();
        global.merge_project(project);
        let merged = global.web_search.clone().unwrap();
        assert_eq!(merged.provider.as_deref(), Some("brave"));
        assert_eq!(merged.base_url, None);
        assert_eq!(merged.api_key_env, ["MY_TAVILY_KEY"]);
        assert!(!merged.allow_private_addresses);
        assert!(merged.enabled);

        // A project may turn the web tools off, but not back on.
        let off: Settings = serde_json::from_str(r#"{"webSearch": {"enabled": false}}"#).unwrap();
        global.merge_project(off);
        let merged = global.web_search.clone().unwrap();
        assert!(!merged.enabled);
        assert_eq!(merged.provider.as_deref(), Some("brave"), "provider kept");
        let on: Settings = serde_json::from_str(r#"{"webSearch": {"enabled": true}}"#).unwrap();
        global.merge_project(on);
        assert!(!global.web_search.unwrap().enabled);
    }

    #[test]
    fn project_web_fetch_cannot_enable_private_addresses_or_reset_global() {
        use loop_agent::harness::{WebFetchConfig, WebFetchProvider};

        let mut global = Settings {
            web_fetch: Some(WebFetchConfig {
                allow_private_addresses: false,
                provider: WebFetchProvider::Soket,
            }),
            ..Default::default()
        };
        let project: Settings = serde_json::from_str(
            r#"{"webFetch":{"allowPrivateAddresses":true,"provider":"direct"}}"#,
        )
        .unwrap();
        global.merge_project(project);
        let merged = global.web_fetch.unwrap();
        assert!(!merged.allow_private_addresses);
        assert_eq!(merged.provider, WebFetchProvider::Direct);

        let mut global = Settings {
            web_fetch: Some(WebFetchConfig {
                allow_private_addresses: false,
                provider: WebFetchProvider::Soket,
            }),
            ..Default::default()
        };
        global.merge_project(Settings::default());
        assert_eq!(
            global.web_fetch,
            Some(WebFetchConfig {
                allow_private_addresses: false,
                provider: WebFetchProvider::Soket,
            })
        );
    }

    #[test]
    fn saved_partial_permissions_gain_new_defaults() {
        let agent_dir = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let path = settings_path(agent_dir.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, r#"{"toolPermissions":{"bash":"allow"}}"#).unwrap();

        let loaded = load_settings(agent_dir.path(), cwd.path(), false).unwrap();
        assert_eq!(
            loaded.tool_permissions.get("bash").map(String::as_str),
            Some("allow")
        );
        assert_eq!(
            loaded.tool_permissions.get("web_fetch").map(String::as_str),
            Some("ask")
        );
        assert_eq!(
            loaded
                .tool_permissions
                .get("web_search")
                .map(String::as_str),
            Some("ask")
        );
    }

    #[test]
    fn enabled_mcp_entries_skip_disabled_names() {
        let mut servers = BTreeMap::new();
        servers.insert(
            "filesystem".into(),
            McpServerConfig {
                command: Some("npx".into()),
                args: vec!["server".into()],
                env: BTreeMap::new(),
                url: None,
                headers: BTreeMap::new(),
            },
        );
        servers.insert(
            "remote".into(),
            McpServerConfig {
                command: None,
                args: Vec::new(),
                env: BTreeMap::new(),
                url: Some("http://127.0.0.1:3100/mcp".into()),
                headers: BTreeMap::new(),
            },
        );
        let entries = crate::runtime::enabled_mcp_server_entries(&servers, &["remote".into()]);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "filesystem");
    }
}
