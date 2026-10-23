//! Worker trait and context for task execution.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::planner::task_graph::{TaskId, TaskNode};
use crate::workflow::types::{Signal, TaskResult};

/// Context provided to a worker when executing a task.
pub struct WorkerContext {
    /// Access to workflow-scoped shared memory.
    pub shared_memory: Arc<dyn SharedMemoryAccess>,
    /// Task-scoped memory.
    pub task_memory: Arc<dyn TaskMemoryAccess>,
    /// Receiver for signals directed at this task.
    pub signal_rx: broadcast::Receiver<Signal>,
    /// Cancellation token for this task.
    pub cancel: CancellationToken,
    /// Artifact store for recording task outputs.
    pub artifact_store: Arc<dyn ArtifactAccess>,
    /// Results from dependency tasks (keyed by task_id).
    pub dependency_results: std::collections::HashMap<TaskId, TaskResult>,
}

/// Error from a worker during execution.
#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    /// Task execution logic failed.
    #[error("task execution failed: {0}")]
    ExecutionFailed(String),
    /// Task was cancelled.
    #[error("task cancelled")]
    Cancelled,
    /// A permission check refused the task; retrying would ask again.
    #[error("task blocked: {0}")]
    Blocked(String),
    /// Task exceeded its timeout.
    #[error("task timed out")]
    TimedOut,
    /// No worker registered for this task kind.
    #[error("unsupported task kind: {0}")]
    UnsupportedKind(String),
    /// Other error.
    #[error("{0}")]
    Other(String),
}

/// A worker executes tasks of specific kinds.
#[async_trait]
pub trait Worker: Send + Sync {
    /// Which task kind strings this worker handles.
    fn supported_task_kinds(&self) -> &[&str];

    /// Execute a task and return its result.
    async fn execute(
        &self,
        task: &TaskNode,
        context: WorkerContext,
    ) -> Result<TaskResult, WorkerError>;
}

/// Trait for shared memory access (decoupled from concrete implementation).
#[async_trait]
pub trait SharedMemoryAccess: Send + Sync {
    /// Get a value by key.
    async fn get(&self, key: &str) -> Option<Value>;
    /// Set a value (writer identifies the task).
    async fn set(&self, key: &str, value: Value, writer: &str);
    /// List keys matching a prefix.
    async fn list_keys(&self, prefix: &str) -> Vec<String>;
    /// Delete a key. Returns whether it existed.
    async fn delete(&self, key: &str) -> bool;
}

/// Trait for task-scoped memory access.
#[async_trait]
pub trait TaskMemoryAccess: Send + Sync {
    /// Get a value by key.
    async fn get(&self, key: &str) -> Option<Value>;
    /// Set a value.
    async fn set(&self, key: &str, value: Value);
    /// List all keys.
    async fn list_keys(&self) -> Vec<String>;
    /// Delete a key. Returns whether it existed.
    async fn delete(&self, key: &str) -> bool;
}

/// Trait for artifact storage access.
#[async_trait]
pub trait ArtifactAccess: Send + Sync {
    /// Store an artifact produced by a task.
    async fn store(&self, task_id: &str, kind: &str, data: Value, path: Option<String>);
    /// Get artifacts produced by a task.
    async fn get_for_task(&self, task_id: &str) -> Vec<Value>;
}
