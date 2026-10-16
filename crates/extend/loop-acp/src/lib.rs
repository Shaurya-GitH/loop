//! Run any Agent Client Protocol (ACP) agent as a Loop backend.
//!
//! [`AcpBackend`] implements [`loop_protocol::AgentBackend`] for an external
//! agent reached over ACP v1: Codex through `codex-acp`, Claude through
//! `claude-agent-acp`, Gemini CLI, Hermes and any other ACP agent. Nothing in
//! this crate depends on which agent it talks to; an agent is just an
//! [`AgentConfig`].

#![deny(missing_docs)]

mod backend;
pub mod mapping;
pub mod process;
pub mod rpc;

pub use agent_client_protocol_schema as schema;
pub use backend::AcpBackend;
pub use process::{AgentConfig, AgentLink, AgentProcess, Connector, ProcessConnector};
