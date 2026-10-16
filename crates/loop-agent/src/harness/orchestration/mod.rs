//! Orchestration adapter: bridges `loop-orchestration` with `loop-agent` types.
//!
//! This module provides concrete worker implementations (`AgentWorker`,
//! `ShellWorker`, `SubWorkflowWorker`) that plug into the orchestration
//! scheduler, plus memory tool builders that wrap orchestration traits
//! into `AgentTool` instances.

pub mod agent_worker;
pub mod sub_workflow_worker;
pub mod tools;

pub use agent_worker::{create_spawn_task_tool, AgentWorker, ShellWorker, WorkflowToolHooks};
pub use sub_workflow_worker::SubWorkflowWorker;
pub use tools::{create_memory_list_tool, create_memory_read_tool, create_memory_write_tool};

/// Progress events emitted during workflow execution for UI display.
#[derive(Debug, Clone)]
pub enum WorkflowProgressEvent {
    /// The planner produced a task graph.
    GraphPlanned {
        /// Compact outline of tasks and dependencies.
        outline: String,
        /// Mermaid `graph TD` source.
        mermaid: String,
    },
    /// A task has started executing.
    TaskStarted {
        /// Task identifier.
        task_id: String,
        /// Human-readable task description.
        description: String,
    },
    /// A task completed successfully.
    TaskCompleted {
        /// Task identifier.
        task_id: String,
        /// Output preview text.
        output: String,
    },
    /// A task failed.
    TaskFailed {
        /// Task identifier.
        task_id: String,
        /// Error description.
        error: String,
    },
}
