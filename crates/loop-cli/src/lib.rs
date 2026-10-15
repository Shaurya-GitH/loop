//! Loop CLI library (shared by the `loop` binary and tests).

#![allow(missing_docs)]

pub mod app;
pub mod clipboard;
pub mod commands;
pub mod debug_log;
pub mod keybindings;
pub mod mcp_serve;
pub mod print_mode;
pub mod provider_setup;
pub mod setup_prompt;
#[cfg(feature = "telemetry")]
pub mod tracing_setup;
pub mod tui;
pub mod wizard;

pub use loop_app_core::{
    self as config, bootstrap, build_models, build_tools, extensions, hooks_load,
    mcp_server_entries, resources, runtime, system_prompt, theme, tool_approval, BootstrapOpts,
    Runtime, ToolApprovalBridge,
};

/// CLI runtime with terminal keybindings.
pub struct CliRuntime {
    /// Shared application runtime.
    pub inner: Runtime,
    /// Terminal keybindings.
    pub keybindings: keybindings::Keybindings,
    /// Debug session logging enabled.
    pub debug: bool,
    /// Path to the active debug log file, if any.
    pub debug_log_path: Option<std::path::PathBuf>,
    /// External agent to start once the surface is ready.
    pub pending_agent: Option<PendingAgent>,
    /// Subscription being started (`id` or `id/model`), until it is ready or fails.
    pub starting_agent: Option<String>,
    /// Backend instances whose updates are already forwarded to the surface.
    pub forwarded: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
}

/// An external agent to start when the surface is ready.
#[derive(Debug, Clone)]
pub enum PendingAgent {
    /// Open a new session with this subscription, on this model.
    New {
        /// Subscription id.
        id: String,
        /// Model id; `None` keeps the agent's default.
        model: Option<String>,
    },
    /// Reopen this recorded session.
    Resume(loop_app_core::agents::AgentSessionRecord),
}

impl std::ops::Deref for CliRuntime {
    type Target = Runtime;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl std::ops::DerefMut for CliRuntime {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

/// Bootstrap CLI runtime (adds keybindings to shared core).
pub async fn bootstrap_cli(opts: BootstrapOpts) -> anyhow::Result<CliRuntime> {
    let inner = bootstrap(opts).await?;
    let keybindings =
        keybindings::Keybindings::load(&config::paths::keybindings_path(&inner.agent_dir))?;
    Ok(CliRuntime {
        inner,
        keybindings,
        debug: false,
        debug_log_path: None,
        pending_agent: None,
        starting_agent: None,
        forwarded: Default::default(),
    })
}
