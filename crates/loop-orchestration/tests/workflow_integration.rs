//! Integration test: full workflow lifecycle with mock workers.

#![allow(clippy::needless_update)]

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use loop_orchestration::memory::bus::create_memory_bus;
use loop_orchestration::memory::SharedMemory;
use loop_orchestration::planner::task_graph::*;
use loop_orchestration::planner::ManualPlanner;
use loop_orchestration::scheduler::worker::*;
use loop_orchestration::scheduler::{Scheduler, SchedulerConfig, WorkerPool};
use loop_orchestration::workflow::event_log::{EventLog, MemoryEventLog};
use loop_orchestration::workflow::signals::SignalRouter;
use loop_orchestration::workflow::types::*;
use loop_orchestration::workflow::WorkflowEngine;
use serde_json::Value;

// ── Mock Workers ────────────────────────────────────────────────────

struct EchoWorker;

#[async_trait]
impl Worker for EchoWorker {
    fn supported_task_kinds(&self) -> &[&str] {
        &["agent_turn"]
    }

    async fn execute(
        &self,
        task: &TaskNode,
        ctx: WorkerContext,
    ) -> Result<TaskResult, WorkerError> {
        if ctx.cancel.is_cancelled() {
            return Err(WorkerError::Cancelled);
        }
        let prompt = match &task.kind {
            TaskKind::AgentTurn { prompt, .. } => prompt.clone(),
            _ => "unknown".to_string(),
        };

        ctx.shared_memory
            .set(
                &format!("output:{}", task.id),
                Value::String(prompt.clone()),
                &task.id,
            )
            .await;

        Ok(TaskResult::with_output(Value::String(format!(
            "echo: {prompt}"
        ))))
    }
}

struct MockShellWorker;

#[async_trait]
impl Worker for MockShellWorker {
    fn supported_task_kinds(&self) -> &[&str] {
        &["shell_command"]
    }

    async fn execute(
        &self,
        task: &TaskNode,
        _ctx: WorkerContext,
    ) -> Result<TaskResult, WorkerError> {
        let cmd = match &task.kind {
            TaskKind::ShellCommand { command } => command.clone(),
            _ => "".to_string(),
        };
        Ok(TaskResult::with_output(serde_json::json!({
            "stdout": format!("mock: {cmd}"),
            "exit_code": 0,
        })))
    }
}

struct FailOnceWorker {
    attempts: AtomicU32,
}

impl FailOnceWorker {
    fn new() -> Self {
        Self {
            attempts: AtomicU32::new(0),
        }
    }
}

#[async_trait]
impl Worker for FailOnceWorker {
    fn supported_task_kinds(&self) -> &[&str] {
        &["agent_turn"]
    }

    async fn execute(
        &self,
        _task: &TaskNode,
        _ctx: WorkerContext,
    ) -> Result<TaskResult, WorkerError> {
        let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
        if attempt == 0 {
            Err(WorkerError::ExecutionFailed("transient failure".into()))
        } else {
            Ok(TaskResult::with_output(Value::String(format!(
                "succeeded on attempt {}",
                attempt + 1
            ))))
        }
    }
}

struct BlockedWorker {
    attempts: AtomicU32,
}

#[async_trait]
impl Worker for BlockedWorker {
    fn supported_task_kinds(&self) -> &[&str] {
        &["agent_turn"]
    }

    async fn execute(
        &self,
        _task: &TaskNode,
        _ctx: WorkerContext,
    ) -> Result<TaskResult, WorkerError> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        Err(WorkerError::Blocked("user rejected the command".into()))
    }
}

struct AlwaysFailWorker;

#[async_trait]
impl Worker for AlwaysFailWorker {
    fn supported_task_kinds(&self) -> &[&str] {
        &["agent_turn"]
    }

    async fn execute(
        &self,
        _task: &TaskNode,
        _ctx: WorkerContext,
    ) -> Result<TaskResult, WorkerError> {
        Err(WorkerError::ExecutionFailed("permanent failure".into()))
    }
}

// ── Helpers ─────────────────────────────────────────────────────────

fn create_engine() -> (Arc<WorkflowEngine>, Arc<MemoryEventLog>) {
    let event_log = Arc::new(MemoryEventLog::new());
    let signal_router = Arc::new(SignalRouter::new());
    let engine = Arc::new(
        WorkflowEngine::new(event_log.clone() as Arc<dyn EventLog>)
            .with_signal_router(signal_router),
    );
    (engine, event_log)
}

fn create_shared_memory() -> Arc<SharedMemory> {
    Arc::new(SharedMemory::new(create_memory_bus()))
}

async fn run_workflow(
    graph: TaskGraph,
    workers: Vec<Arc<dyn Worker>>,
    config: SchedulerConfig,
) -> Result<WorkflowResult, String> {
    let (engine, _log) = create_engine();
    let shared_memory = create_shared_memory();

    let mut pool = WorkerPool::new(config.max_concurrency);
    for w in workers {
        pool.register(w);
    }

    let wf_id = "test_wf";
    engine
        .start_workflow(wf_id.to_string(), graph)
        .await
        .map_err(|e| e.to_string())?;

    let scheduler = Scheduler::new(engine, pool, shared_memory, config);
    scheduler.run(wf_id).await.map_err(|e| e.to_string())
}

// ── Tests ───────────────────────────────────────────────────────────

#[tokio::test]
async fn single_agent_task() {
    let mut p = ManualPlanner::new();
    p.add_agent_turn("greet", "say hello");
    let graph = p.build().unwrap();

    let result = run_workflow(
        graph,
        vec![Arc::new(EchoWorker)],
        SchedulerConfig::default(),
    )
    .await
    .unwrap();

    assert!(result.success);
}

#[tokio::test]
async fn linear_chain_executes_in_order() {
    let mut p = ManualPlanner::new();
    let t1 = p.add_agent_turn("step 1", "first");
    let t2 = p.add_agent_turn("step 2", "second");
    let t3 = p.add_agent_turn("step 3", "third");
    p.depends_on(&t2, &t1);
    p.depends_on(&t3, &t2);
    let graph = p.build().unwrap();

    let result = run_workflow(
        graph,
        vec![Arc::new(EchoWorker)],
        SchedulerConfig::default(),
    )
    .await
    .unwrap();

    assert!(result.success);
    assert_eq!(result.task_results.len(), 3);
}

#[tokio::test]
async fn parallel_tasks_all_complete() {
    let mut p = ManualPlanner::new();
    p.add_agent_turn("task a", "parallel a");
    p.add_agent_turn("task b", "parallel b");
    p.add_agent_turn("task c", "parallel c");
    let graph = p.build().unwrap();

    let result = run_workflow(
        graph,
        vec![Arc::new(EchoWorker)],
        SchedulerConfig::default(),
    )
    .await
    .unwrap();

    assert!(result.success);
    assert_eq!(result.task_results.len(), 3);
    let output = result.output_text();
    assert!(
        output.contains("echo:"),
        "expected aggregated task output, got: {output:?}"
    );
}

#[tokio::test]
async fn diamond_workflow() {
    let mut p = ManualPlanner::new();
    let root = p.add_agent_turn("design", "design API");
    let left = p.add_agent_turn("implement", "write code");
    let right = p.add_shell_command("scaffold", "mkdir -p src");
    let join = p.add_barrier("merge");
    p.depends_on(&left, &root);
    p.depends_on(&right, &root);
    p.depends_on(&join, &left);
    p.depends_on(&join, &right);
    let graph = p.build().unwrap();

    let result = run_workflow(
        graph,
        vec![Arc::new(EchoWorker), Arc::new(MockShellWorker)],
        SchedulerConfig::default(),
    )
    .await
    .unwrap();

    assert!(result.success);
}

#[tokio::test]
async fn retry_succeeds_on_second_attempt() {
    let mut p = ManualPlanner::new();
    p.add_agent_turn_with_config(
        "flaky task",
        "try me",
        None,
        None,
        TaskConfig {
            max_retries: 2,
            timeout_ms: 0,
            priority: 0,
        },
    );
    let graph = p.build().unwrap();

    let result = run_workflow(
        graph,
        vec![Arc::new(FailOnceWorker::new())],
        SchedulerConfig::default(),
    )
    .await
    .unwrap();

    assert!(result.success);
}

#[tokio::test]
async fn retry_exhaustion_fails_task() {
    let mut p = ManualPlanner::new();
    p.add_agent_turn_with_config(
        "always fails",
        "doom",
        None,
        None,
        TaskConfig {
            max_retries: 1,
            timeout_ms: 0,
            priority: 0,
        },
    );
    let graph = p.build().unwrap();

    let result = run_workflow(
        graph,
        vec![Arc::new(AlwaysFailWorker)],
        SchedulerConfig::default(),
    )
    .await
    .unwrap();

    assert!(!result.success);
}

/// A refused task fails at once: retrying would ask the user again.
#[tokio::test]
async fn blocked_task_is_not_retried() {
    let mut p = ManualPlanner::new();
    p.add_agent_turn_with_config(
        "blocked",
        "rm -rf build",
        None,
        None,
        TaskConfig {
            max_retries: 3,
            timeout_ms: 0,
            priority: 0,
        },
    );
    let graph = p.build().unwrap();
    let worker = Arc::new(BlockedWorker {
        attempts: AtomicU32::new(0),
    });

    let result = run_workflow(
        graph,
        vec![Arc::clone(&worker) as Arc<dyn Worker>],
        SchedulerConfig::default(),
    )
    .await
    .unwrap();

    assert!(!result.success);
    assert_eq!(worker.attempts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn fail_fast_stops_early() {
    let mut p = ManualPlanner::new();
    p.add_agent_turn_with_config(
        "will fail",
        "fail",
        None,
        None,
        TaskConfig {
            max_retries: 0,
            timeout_ms: 0,
            priority: 0,
        },
    );
    let graph = p.build().unwrap();

    let config = SchedulerConfig {
        max_concurrency: 4,
        fail_fast: true,
    };

    let result = run_workflow(graph, vec![Arc::new(AlwaysFailWorker)], config)
        .await
        .unwrap();

    assert!(!result.success);
}

#[tokio::test]
async fn shared_memory_written_by_workers() {
    let (engine, _log) = create_engine();
    let shared_memory = create_shared_memory();

    let mut p = ManualPlanner::new();
    p.add_agent_turn("write to memory", "remember this");
    let graph = p.build().unwrap();

    let mut pool = WorkerPool::new(4);
    pool.register(Arc::new(EchoWorker));

    let wf_id = "mem_test";
    engine
        .start_workflow(wf_id.to_string(), graph)
        .await
        .unwrap();

    let scheduler = Scheduler::new(
        engine,
        pool,
        shared_memory.clone(),
        SchedulerConfig::default(),
    );
    let result = scheduler.run(wf_id).await.unwrap();
    assert!(result.success);

    let keys = shared_memory.list("output:").await;
    assert!(!keys.is_empty());
}

#[tokio::test]
async fn mixed_worker_types() {
    let mut p = ManualPlanner::new();
    let agent = p.add_agent_turn("think", "analyze the problem");
    let shell = p.add_shell_command("build", "cargo build");
    p.depends_on(&shell, &agent);
    let graph = p.build().unwrap();

    let result = run_workflow(
        graph,
        vec![Arc::new(EchoWorker), Arc::new(MockShellWorker)],
        SchedulerConfig::default(),
    )
    .await
    .unwrap();

    assert!(result.success);
    assert_eq!(result.task_results.len(), 2);
}

#[tokio::test]
async fn workflow_engine_event_replay() {
    let (engine, log) = create_engine();

    let mut p = ManualPlanner::new();
    p.add_agent_turn("task", "hello");
    let graph = p.build().unwrap();
    let wf_id = "replay_test";

    engine
        .start_workflow(wf_id.to_string(), graph)
        .await
        .unwrap();

    let events = log.read(wf_id, 0).await.unwrap();
    assert!(events.len() >= 2); // WorkflowStarted + TaskScheduled

    let state = engine.state(wf_id).await.unwrap();
    assert_eq!(state.status, WorkflowStatus::Running);
    assert!(!state.ready_tasks().is_empty());
}

#[tokio::test]
async fn cancellation_stops_workflow() {
    let (engine, _log) = create_engine();
    let shared_memory = create_shared_memory();

    let mut p = ManualPlanner::new();
    p.add_agent_turn("task", "hello");
    let graph = p.build().unwrap();

    let mut pool = WorkerPool::new(4);
    pool.register(Arc::new(EchoWorker));

    let wf_id = "cancel_test";
    engine
        .start_workflow(wf_id.to_string(), graph)
        .await
        .unwrap();

    let scheduler = Scheduler::new(engine, pool, shared_memory, SchedulerConfig::default());
    scheduler.cancel();

    let result = scheduler.run(wf_id).await;
    assert!(result.is_err());
}
