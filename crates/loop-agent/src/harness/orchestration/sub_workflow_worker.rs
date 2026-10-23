//! Sub-workflow worker: executes nested task graphs as a single task.

use std::sync::Arc;

use async_trait::async_trait;
use loop_ai::Model;
use loop_orchestration::planner::task_graph::{TaskKind, TaskNode};
use loop_orchestration::scheduler::worker::{Worker, WorkerContext, WorkerError};
use loop_orchestration::scheduler::{Scheduler, SchedulerConfig, WorkerPool};
use loop_orchestration::workflow::engine::WorkflowEngine;
use loop_orchestration::workflow::event_log::{EventLog, MemoryEventLog};
use loop_orchestration::workflow::signals::SignalRouter;
use loop_orchestration::workflow::types::TaskResult;

use super::agent_worker::{AgentWorker, ShellWorker, WorkflowToolHooks};
use crate::harness::types::ExecutionEnv;
use crate::stream_fn::StreamFn;
use crate::types::AgentTool;

/// Worker that executes `SubWorkflow` tasks by running a nested scheduler.
pub struct SubWorkflowWorker {
    stream_fn: StreamFn,
    host_env: Arc<dyn ExecutionEnv>,
    base_tools: Vec<AgentTool>,
    default_model: Model,
    system_prompt: String,
    hooks: WorkflowToolHooks,
}

impl SubWorkflowWorker {
    /// Create a new sub-workflow worker.
    pub fn new(
        stream_fn: StreamFn,
        host_env: Arc<dyn ExecutionEnv>,
        base_tools: Vec<AgentTool>,
        default_model: Model,
        system_prompt: String,
    ) -> Self {
        Self {
            stream_fn,
            host_env,
            base_tools,
            default_model,
            system_prompt,
            hooks: WorkflowToolHooks::default(),
        }
    }

    /// Run `hooks` around every tool call in the nested workflow.
    pub fn with_tool_hooks(mut self, hooks: WorkflowToolHooks) -> Self {
        self.hooks = hooks;
        self
    }
}

#[async_trait]
impl Worker for SubWorkflowWorker {
    fn supported_task_kinds(&self) -> &[&str] {
        &["sub_workflow"]
    }

    async fn execute(
        &self,
        task: &TaskNode,
        ctx: WorkerContext,
    ) -> Result<TaskResult, WorkerError> {
        let TaskKind::SubWorkflow { plan } = &task.kind else {
            return Err(WorkerError::UnsupportedKind(format!("{:?}", task.kind)));
        };

        let workflow_id = format!("sub_wf_{}", uuid::Uuid::now_v7());

        let event_log = Arc::new(MemoryEventLog::new());
        let signal_router = Arc::new(SignalRouter::new());
        let engine = Arc::new(
            WorkflowEngine::new(event_log as Arc<dyn EventLog>).with_signal_router(signal_router),
        );

        let agent_worker = Arc::new(
            AgentWorker::new(
                Arc::clone(&self.stream_fn),
                Arc::clone(&self.host_env),
                self.base_tools.clone(),
                self.default_model.clone(),
                self.system_prompt.clone(),
                loop_ai::SimpleStreamOptions::default(),
            )
            .with_tool_hooks(self.hooks.clone()),
        );
        let shell_worker = Arc::new(
            ShellWorker::new(Arc::clone(&self.host_env)).with_tool_hooks(self.hooks.clone()),
        );

        let mut pool = WorkerPool::new(4);
        pool.register(agent_worker);
        pool.register(shell_worker);

        engine
            .start_workflow(workflow_id.clone(), *plan.clone())
            .await
            .map_err(|e| WorkerError::ExecutionFailed(e.to_string()))?;

        let scheduler = Scheduler::new(engine, pool, ctx.shared_memory, SchedulerConfig::default());

        let cancel = ctx.cancel.clone();
        let sched_cancel = scheduler.cancel_token();
        tokio::spawn(async move {
            cancel.cancelled().await;
            sched_cancel.cancel();
        });

        let result = scheduler
            .run(&workflow_id)
            .await
            .map_err(|e| WorkerError::ExecutionFailed(e.to_string()))?;

        Ok(TaskResult {
            output: result.output,
            artifacts: Vec::new(),
            messages: Vec::new(),
        })
    }
}
