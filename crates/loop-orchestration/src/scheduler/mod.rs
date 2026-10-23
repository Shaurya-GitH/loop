//! Scheduler: dispatch loop coordinating workers on a task graph.
//!
//! Execution scheduling, worker coordination, and artifact management.

pub mod artifacts;
pub mod pool;
pub mod worker;

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

pub use artifacts::{ArtifactStore, ScopedArtifactAccess};
pub use pool::WorkerPool;
pub use worker::{
    ArtifactAccess, SharedMemoryAccess, TaskMemoryAccess, Worker, WorkerContext, WorkerError,
};

use crate::planner::task_graph::{TaskId, TaskKind, TaskNode, TaskStatus};
use crate::workflow::engine::WorkflowEngine;
use crate::workflow::types::{TaskResult, WorkflowError, WorkflowResult};

/// Error from the scheduler.
#[derive(Debug, thiserror::Error)]
pub enum SchedulerError {
    /// Underlying workflow error.
    #[error("workflow error: {0}")]
    Workflow(#[from] WorkflowError),
    /// Worker execution error.
    #[error("worker error: {0}")]
    Worker(#[from] WorkerError),
    /// Scheduler was cancelled.
    #[error("cancelled")]
    Cancelled,
    /// Other error.
    #[error("{0}")]
    Other(String),
}

/// Configuration for the scheduler.
#[derive(Debug, Clone)]
pub struct SchedulerConfig {
    /// Maximum concurrent tasks.
    pub max_concurrency: usize,
    /// Whether to fail the workflow on first task failure.
    pub fail_fast: bool,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            max_concurrency: 4,
            fail_fast: false,
        }
    }
}

/// The scheduler drives task execution across the worker pool.
pub struct Scheduler {
    workflow_engine: Arc<WorkflowEngine>,
    worker_pool: Arc<RwLock<WorkerPool>>,
    artifact_store: Arc<ArtifactStore>,
    shared_memory: Arc<dyn SharedMemoryAccess>,
    cancel: CancellationToken,
    config: SchedulerConfig,
}

impl Scheduler {
    /// Create a new scheduler.
    pub fn new(
        workflow_engine: Arc<WorkflowEngine>,
        worker_pool: WorkerPool,
        shared_memory: Arc<dyn SharedMemoryAccess>,
        config: SchedulerConfig,
    ) -> Self {
        Self {
            workflow_engine,
            worker_pool: Arc::new(RwLock::new(worker_pool)),
            artifact_store: Arc::new(ArtifactStore::new()),
            shared_memory,
            cancel: CancellationToken::new(),
            config,
        }
    }

    /// Get the cancellation token for this scheduler.
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// Access the artifact store.
    pub fn artifact_store(&self) -> &Arc<ArtifactStore> {
        &self.artifact_store
    }

    /// Run the scheduler loop for a workflow until completion or failure.
    pub async fn run(&self, workflow_id: &str) -> Result<WorkflowResult, SchedulerError> {
        let progress = self.workflow_engine.progress_notify();

        loop {
            if self.cancel.is_cancelled() {
                return Err(SchedulerError::Cancelled);
            }

            let state = self.workflow_engine.state(workflow_id).await?;

            if state.is_complete() {
                let result = self.workflow_engine.result(workflow_id).await?;
                self.workflow_engine
                    .complete_workflow(workflow_id, result.clone())
                    .await?;
                return Ok(result);
            }

            if self.config.fail_fast && state.has_failures() {
                let result = WorkflowResult {
                    success: false,
                    output: serde_json::json!({ "error": "task failure in fail-fast mode" }),
                    task_results: Vec::new(),
                    failed_tasks: Vec::new(),
                    total_task_count: state.task_statuses.len(),
                };
                self.workflow_engine
                    .complete_workflow(workflow_id, result.clone())
                    .await?;
                return Ok(result);
            }

            let ready = state.ready_tasks();
            if ready.is_empty() {
                tokio::select! {
                    _ = progress.notified() => continue,
                    _ = self.cancel.cancelled() => return Err(SchedulerError::Cancelled),
                }
            }

            for task_id in ready {
                let task_node = state.graph.tasks.get(&task_id).cloned();
                let Some(task_node) = task_node else {
                    continue;
                };

                let engine = Arc::clone(&self.workflow_engine);
                let pool = Arc::clone(&self.worker_pool);
                let artifact_store = Arc::clone(&self.artifact_store);
                let shared_memory = Arc::clone(&self.shared_memory);
                let cancel = self.cancel.clone();
                let wf_id = workflow_id.to_string();
                let dep_results = self.collect_dependency_results(&state, &task_id);
                let max_retries = task_node.config.max_retries;

                let permit = {
                    let pool_guard = pool.read().await;
                    match pool_guard.acquire_permit().await {
                        Ok(p) => p,
                        Err(_) => continue,
                    }
                };

                tokio::spawn(async move {
                    let _permit = permit;
                    Self::execute_task(
                        engine,
                        pool,
                        artifact_store,
                        shared_memory,
                        cancel,
                        wf_id,
                        task_node,
                        dep_results,
                        max_retries,
                    )
                    .await;
                });
            }

            tokio::select! {
                _ = progress.notified() => {}
                _ = self.cancel.cancelled() => return Err(SchedulerError::Cancelled),
            }
        }
    }

    fn collect_dependency_results(
        &self,
        state: &crate::workflow::types::WorkflowState,
        task_id: &str,
    ) -> HashMap<TaskId, TaskResult> {
        let deps = state.graph.dependencies_of(task_id);
        let mut results = HashMap::new();
        for dep_id in deps {
            if let Some(TaskStatus::Completed(result)) = state.task_statuses.get(&dep_id) {
                results.insert(dep_id, result.clone());
            }
        }

        for edge in &state.graph.edges {
            if let crate::planner::task_graph::TaskEdge::DataFlow { from, to, key } = edge {
                if to == task_id {
                    if let Some(TaskStatus::Completed(result)) = state.task_statuses.get(from) {
                        let keyed = TaskResult {
                            output: serde_json::json!({ key: result.output }),
                            artifacts: result.artifacts.clone(),
                            messages: result.messages.clone(),
                        };
                        results
                            .entry(from.clone())
                            .and_modify(|existing: &mut TaskResult| {
                                if let Some(obj) = existing.output.as_object_mut() {
                                    obj.insert(key.clone(), result.output.clone());
                                }
                            })
                            .or_insert(keyed);
                    }
                }
            }
        }

        results
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "pre-existing signature; refactor tracked separately"
    )]
    async fn execute_task(
        engine: Arc<WorkflowEngine>,
        pool: Arc<RwLock<WorkerPool>>,
        artifact_store: Arc<ArtifactStore>,
        shared_memory: Arc<dyn SharedMemoryAccess>,
        cancel: CancellationToken,
        workflow_id: String,
        task_node: TaskNode,
        dependency_results: HashMap<TaskId, TaskResult>,
        max_retries: u32,
    ) {
        let task_id = task_node.id.clone();
        let worker_id = format!("worker_{}", uuid::Uuid::now_v7());

        if let Err(e) = engine
            .task_started(&workflow_id, &task_id, &worker_id)
            .await
        {
            tracing::error!("Failed to mark task started: {e}");
            return;
        }

        let kind_str = match &task_node.kind {
            TaskKind::AgentTurn { .. } => "agent_turn",
            TaskKind::ShellCommand { .. } => "shell_command",
            TaskKind::SubWorkflow { .. } => "sub_workflow",
            TaskKind::Barrier => "barrier",
            TaskKind::Custom { worker_type, .. } => worker_type.as_str(),
        };

        if matches!(task_node.kind, TaskKind::Barrier) {
            let _ = engine
                .task_completed(&workflow_id, &task_id, TaskResult::empty())
                .await;
            return;
        }

        let worker = {
            let pool_guard = pool.read().await;
            match pool_guard.find_worker(kind_str) {
                Ok(w) => w,
                Err(e) => {
                    let _ = engine
                        .task_failed(&workflow_id, &task_id, e.to_string(), 0)
                        .await;
                    return;
                }
            }
        };

        let mut attempt: u32 = 0;
        loop {
            let signal_rx = engine.signal_router().register_task(&task_id).await;
            let task_cancel = cancel.child_token();
            let task_memory = Arc::new(InMemoryTaskMemory::new(task_id.clone()));

            let context = WorkerContext {
                shared_memory: Arc::clone(&shared_memory),
                task_memory,
                signal_rx,
                cancel: task_cancel.clone(),
                artifact_store: Arc::new(ScopedArtifactAccess::new(
                    Arc::clone(&artifact_store),
                    task_id.clone(),
                )),
                dependency_results: dependency_results.clone(),
            };

            let timeout_ms = task_node.config.timeout_ms;
            let result = if timeout_ms > 0 {
                let duration = std::time::Duration::from_millis(timeout_ms);
                match tokio::time::timeout(duration, worker.execute(&task_node, context)).await {
                    Ok(result) => result,
                    Err(_) => {
                        task_cancel.cancel();
                        Err(WorkerError::TimedOut)
                    }
                }
            } else {
                worker.execute(&task_node, context).await
            };

            match result {
                Ok(task_result) => {
                    let _ = engine
                        .task_completed(&workflow_id, &task_id, task_result)
                        .await;
                    return;
                }
                Err(WorkerError::Cancelled) => {
                    let _ = engine
                        .task_cancelled(&workflow_id, &task_id, "cancelled".to_string())
                        .await;
                    return;
                }
                Err(e) => {
                    let retryable = !matches!(e, WorkerError::Blocked(_));
                    if retryable && attempt < max_retries && !cancel.is_cancelled() {
                        attempt += 1;
                        tracing::warn!(
                            task_id = %task_id,
                            attempt,
                            max_retries,
                            "Task failed, retrying: {e}"
                        );
                        continue;
                    }
                    let _ = engine
                        .task_failed(&workflow_id, &task_id, e.to_string(), attempt)
                        .await;
                    return;
                }
            }
        }
    }

    /// Cancel the workflow execution.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }
}

/// Simple in-memory task memory implementation.
struct InMemoryTaskMemory {
    #[allow(dead_code)]
    task_id: TaskId,
    store: tokio::sync::RwLock<HashMap<String, serde_json::Value>>,
}

impl InMemoryTaskMemory {
    fn new(task_id: TaskId) -> Self {
        Self {
            task_id,
            store: tokio::sync::RwLock::new(HashMap::new()),
        }
    }
}

#[async_trait::async_trait]
impl TaskMemoryAccess for InMemoryTaskMemory {
    async fn get(&self, key: &str) -> Option<serde_json::Value> {
        self.store.read().await.get(key).cloned()
    }

    async fn set(&self, key: &str, value: serde_json::Value) {
        self.store.write().await.insert(key.to_string(), value);
    }

    async fn list_keys(&self) -> Vec<String> {
        self.store.read().await.keys().cloned().collect()
    }

    async fn delete(&self, key: &str) -> bool {
        self.store.write().await.remove(key).is_some()
    }
}
