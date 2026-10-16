//! Shared bootstrap, configuration, and runtime wiring for Loop CLI and desktop.

pub mod agents;
pub mod config;
pub mod extensions;
pub mod hooks_load;
pub mod interaction;
pub mod model_selection;
pub mod resources;
pub mod runtime;
pub mod system_prompt;
pub mod theme;
pub mod tool_approval;
pub mod web_tools;

pub use config::*;
pub use runtime::{
    bootstrap, build_models, build_tools, mcp_server_entries, BootstrapOpts, ConnectedProvider,
    Runtime,
};
pub use tool_approval::ToolApprovalBridge;
