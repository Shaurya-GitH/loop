//! Agent worker: wraps `run_agent_loop` as a scheduler worker.

use std::sync::Arc;

use async_trait::async_trait;
use loop_ai::{AssistantContent, AssistantMessage, Model, SimpleStreamOptions, ToolCall};
use loop_orchestration::planner::task_graph::{TaskKind, TaskNode};
use loop_orchestration::scheduler::worker::{Worker, WorkerContext, WorkerError};
use loop_orchestration::workflow::types::{Signal, TaskResult};

use super::tools::{create_memory_list_tool, create_memory_read_tool, create_memory_write_tool};
use crate::agent_loop::run_agent_loop;
use crate::harness::types::ExecutionEnv;
use crate::messages::convert_to_llm;
use crate::stream_fn::StreamFn;
use crate::types::{
    AfterToolCallFn, AgentContext, AgentEventSink, AgentLoopConfig, AgentMessage, AgentTool,
    AgentToolResult, BeforeToolCallContext, BeforeToolCallFn, BeforeToolCallResult,
};

/// The host's tool-call hooks, applied to every tool a workflow task runs so
/// that workflows get the same checks as the main chat.
#[derive(Clone, Default)]
pub struct WorkflowToolHooks {
    /// Runs before each tool call; may block it.
    pub before: Option<BeforeToolCallFn>,
    /// Runs after each tool call; may patch the result.
    pub after: Option<AfterToolCallFn>,
}

/// Worker that executes `AgentTurn` tasks by running the agent loop.
pub struct AgentWorker {
    stream_fn: StreamFn,
    host_env: Arc<dyn ExecutionEnv>,
    base_tools: Vec<AgentTool>,
    default_model: Model,
    system_prompt: String,
    stream_options: SimpleStreamOptions,
    hooks: WorkflowToolHooks,
}

impl AgentWorker {
    /// Create a new agent worker.
    pub fn new(
        stream_fn: StreamFn,
        host_env: Arc<dyn ExecutionEnv>,
        base_tools: Vec<AgentTool>,
        default_model: Model,
        system_prompt: String,
        stream_options: SimpleStreamOptions,
    ) -> Self {
        Self {
            stream_fn,
            host_env,
            base_tools,
            default_model,
            system_prompt,
            stream_options,
            hooks: WorkflowToolHooks::default(),
        }
    }

    /// Run `hooks` around every tool call this worker's agent makes.
    pub fn with_tool_hooks(mut self, hooks: WorkflowToolHooks) -> Self {
        self.hooks = hooks;
        self
    }

    fn build_system_prompt(&self, task: &TaskNode) -> String {
        format!(
            "{}\n\n## Current Task\n\nTask ID: {}\nDescription: {}\n\n\
             You are one agent in a multi-agent workflow.\n\
             Your final message MUST contain the result of this task as plain text \
             (a summary of findings). Shared memory is extra coordination, not a substitute \
             for that final message.\n\
             If the task asks you to write a file, use the write tool and confirm the path \
             in your final message.\n\
             A mermaid diagram of this workflow is in shared memory under the key `task_graph` \
             (scope: shared). Read it if you need to include the task graph in a document.",
            self.system_prompt, task.id, task.description
        )
    }
}

#[async_trait]
impl Worker for AgentWorker {
    fn supported_task_kinds(&self) -> &[&str] {
        &["agent_turn"]
    }

    async fn execute(
        &self,
        task: &TaskNode,
        ctx: WorkerContext,
    ) -> Result<TaskResult, WorkerError> {
        let TaskKind::AgentTurn {
            prompt,
            tools: tool_filter,
            model: _model_override,
        } = &task.kind
        else {
            return Err(WorkerError::UnsupportedKind(format!("{:?}", task.kind)));
        };

        let mut agent_tools: Vec<AgentTool> = if let Some(filter) = tool_filter {
            let filtered: Vec<AgentTool> = self
                .base_tools
                .iter()
                .filter(|t| filter.contains(&t.name))
                .cloned()
                .collect();
            if filtered.is_empty() {
                // Filter matched no base tools — include all so the agent
                // retains filesystem access (read/write/edit/bash).
                self.base_tools.clone()
            } else {
                filtered
            }
        } else {
            self.base_tools.clone()
        };

        agent_tools.push(create_memory_read_tool(
            Arc::clone(&ctx.shared_memory),
            Arc::clone(&ctx.task_memory),
        ));
        agent_tools.push(create_memory_write_tool(
            Arc::clone(&ctx.shared_memory),
            Arc::clone(&ctx.task_memory),
            task.id.clone(),
        ));
        agent_tools.push(create_memory_list_tool(
            Arc::clone(&ctx.shared_memory),
            Arc::clone(&ctx.task_memory),
        ));

        let mut initial_messages: Vec<AgentMessage> = Vec::new();
        if !ctx.dependency_results.is_empty() {
            let dep_summary: Vec<String> = ctx
                .dependency_results
                .iter()
                .map(|(id, result)| {
                    let text = result.output_text();
                    if text.is_empty() {
                        format!("- Task '{id}': (no text output)")
                    } else {
                        format!("- Task '{id}':\n{text}")
                    }
                })
                .collect();
            initial_messages.push(AgentMessage::user_text(format!(
                "Context from completed dependency tasks:\n{}",
                dep_summary.join("\n\n")
            )));
        }

        let context = AgentContext {
            system_prompt: self.build_system_prompt(task),
            messages: initial_messages,
            tools: Some(agent_tools),
        };

        let model = self.default_model.clone();
        let mut config = AgentLoopConfig::new(model);
        config.convert_to_llm = Arc::new(|msgs| Box::pin(async move { convert_to_llm(&msgs) }));
        config.stream_options = self.stream_options.clone();
        // Isolate parallel workers from the parent chat session and each other.
        config.stream_options.base.session_id = Some(format!("wf-task-{}", task.id));
        config.before_tool_call = self.hooks.before.clone();
        config.after_tool_call = self.hooks.after.clone();

        let signal_rx = Arc::new(tokio::sync::Mutex::new(ctx.signal_rx));
        let steer_rx = Arc::clone(&signal_rx);
        config.get_steering_messages = Some(Arc::new(move || {
            let rx = Arc::clone(&steer_rx);
            Box::pin(async move {
                let mut messages = Vec::new();
                let mut guard = rx.lock().await;
                while let Ok(signal) = guard.try_recv() {
                    if let Signal::UserSteer { message } = signal {
                        if let Ok(m) = serde_json::from_value::<AgentMessage>(message) {
                            messages.push(m);
                        }
                    }
                }
                messages
            })
        }));

        let emit: AgentEventSink = Arc::new(|_| Box::pin(async {}));

        let prompts = vec![AgentMessage::user_text(prompt.clone())];
        let cancel_token = ctx.cancel.clone();

        let result = run_agent_loop(
            prompts,
            context,
            config,
            emit,
            Some(cancel_token.clone()),
            Some(Arc::clone(&self.stream_fn)),
        )
        .await;

        if cancel_token.is_cancelled() {
            return Err(WorkerError::Cancelled);
        }

        match result {
            Ok(messages) => {
                let mut artifacts = Vec::new();
                collect_file_artifacts(&messages, &*self.host_env, &mut artifacts).await;

                let mut output = extract_output_from_messages(&messages);
                if output_is_empty(&output) && !artifacts.is_empty() {
                    let paths: Vec<&str> =
                        artifacts.iter().filter_map(|a| a.path.as_deref()).collect();
                    output = serde_json::Value::String(format!("Wrote: {}", paths.join(", ")));
                }

                if let Some(reason) = detect_task_failure(&output) {
                    return Err(WorkerError::ExecutionFailed(reason));
                }

                let serialized_messages: Vec<serde_json::Value> = messages
                    .iter()
                    .filter_map(|m| serde_json::to_value(m).ok())
                    .collect();

                Ok(TaskResult {
                    output,
                    artifacts,
                    messages: serialized_messages,
                })
            }
            Err(e) => Err(WorkerError::ExecutionFailed(e.to_string())),
        }
    }
}

/// Worker that executes `ShellCommand` tasks.
pub struct ShellWorker {
    host_env: Arc<dyn ExecutionEnv>,
    hooks: WorkflowToolHooks,
}

impl ShellWorker {
    /// Create a new shell worker.
    pub fn new(host_env: Arc<dyn ExecutionEnv>) -> Self {
        Self {
            host_env,
            hooks: WorkflowToolHooks::default(),
        }
    }

    /// Check each command with `hooks.before`, as if it were a `bash` call.
    pub fn with_tool_hooks(mut self, hooks: WorkflowToolHooks) -> Self {
        self.hooks = hooks;
        self
    }

    /// Why `command` may not run, or `None` when it may: the `bash` tool's
    /// blocklist, then the host's before-tool hook.
    async fn blocked(
        &self,
        task_id: &str,
        command: &str,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Option<String> {
        if let Err(reason) = crate::harness::tools::check_command_policy(command, &[]) {
            return Some(reason);
        }
        let before = self.hooks.before.as_ref()?;
        let tool_call = ToolCall {
            id: format!("wf-shell-{task_id}"),
            name: "bash".into(),
            arguments: serde_json::json!({ "command": command }),
            thought_signature: None,
        };
        let assistant_message = AssistantMessage {
            content: vec![AssistantContent::ToolCall(tool_call.clone())],
            api: String::new(),
            provider: String::new(),
            model: String::new(),
            response_model: None,
            response_id: None,
            usage: Default::default(),
            stop_reason: loop_ai::StopReason::ToolUse,
            error_message: None,
            raw_stop_reason: None,
            timestamp: chrono::Utc::now().timestamp_millis(),
        };
        let context = BeforeToolCallContext {
            assistant_message,
            args: tool_call.arguments.clone(),
            tool_call,
            context: AgentContext::default(),
        };
        match before(context, Some(cancel.clone())).await {
            Some(BeforeToolCallResult {
                block: true,
                reason,
            }) => Some(reason.unwrap_or_else(|| "Tool execution was blocked".into())),
            _ => None,
        }
    }
}

#[async_trait]
impl Worker for ShellWorker {
    fn supported_task_kinds(&self) -> &[&str] {
        &["shell_command"]
    }

    async fn execute(
        &self,
        task: &TaskNode,
        ctx: WorkerContext,
    ) -> Result<TaskResult, WorkerError> {
        let TaskKind::ShellCommand { command } = &task.kind else {
            return Err(WorkerError::UnsupportedKind(format!("{:?}", task.kind)));
        };

        use crate::harness::types::ShellExecOptions;

        if let Some(reason) = self.blocked(&task.id, command, &ctx.cancel).await {
            return Err(WorkerError::Blocked(reason));
        }
        if ctx.cancel.is_cancelled() {
            return Err(WorkerError::Cancelled);
        }

        let options = ShellExecOptions {
            cancel: Some(ctx.cancel.clone()),
            ..Default::default()
        };

        let output = self
            .host_env
            .exec(command, options)
            .await
            .map_err(|e| WorkerError::ExecutionFailed(e.to_string()))?;

        if ctx.cancel.is_cancelled() {
            return Err(WorkerError::Cancelled);
        }

        let success = output.exit_code == 0;
        let result_output = serde_json::json!({
            "stdout": output.stdout,
            "stderr": output.stderr,
            "exit_code": output.exit_code,
        });

        if success {
            Ok(TaskResult::with_output(result_output))
        } else {
            Err(WorkerError::ExecutionFailed(format!(
                "command exited with code {}: {}",
                output.exit_code,
                output.stderr.chars().take(500).collect::<String>()
            )))
        }
    }
}

fn extract_output_from_messages(messages: &[AgentMessage]) -> serde_json::Value {
    let mut texts = Vec::new();
    let mut thinking = Vec::new();
    for msg in messages {
        let Some(asst) = msg.as_assistant() else {
            continue;
        };
        for content in &asst.content {
            match content {
                AssistantContent::Text(t) => {
                    if !t.text.trim().is_empty() {
                        texts.push(t.text.clone());
                    }
                }
                AssistantContent::Thinking(t) => {
                    if !t.thinking.trim().is_empty() {
                        thinking.push(t.thinking.clone());
                    }
                }
                _ => {}
            }
        }
    }
    if !texts.is_empty() {
        serde_json::Value::String(texts.join("\n\n"))
    } else if let Some(last) = thinking.last() {
        serde_json::Value::String(last.clone())
    } else {
        serde_json::Value::Null
    }
}

fn output_is_empty(output: &serde_json::Value) -> bool {
    match output {
        serde_json::Value::Null => true,
        serde_json::Value::String(s) => s.trim().is_empty(),
        serde_json::Value::Array(a) => a.is_empty(),
        serde_json::Value::Object(m) => m.is_empty(),
        _ => false,
    }
}

const FAILURE_INDICATORS: &[&str] = &[
    "blocked:",
    "could not complete",
    "cannot complete",
    "task blocked",
    "no filesystem tools",
    "no file-access tools",
    "tools are all unavailable",
    "inaccessible",
    "could not be produced",
];

/// Scan task output text for signals that the agent reported failure despite
/// the loop returning `Ok`. Returns a trimmed reason string if detected.
fn detect_task_failure(output: &serde_json::Value) -> Option<String> {
    let text = output.as_str()?;
    let lower = text.to_lowercase();
    for &indicator in FAILURE_INDICATORS {
        if lower.contains(indicator) {
            let reason = text
                .lines()
                .find(|l| l.to_lowercase().contains(indicator))
                .unwrap_or("agent reported task blocked / unable to complete")
                .trim();
            return Some(reason.to_string());
        }
    }
    None
}

/// Walk agent messages to find successful `write` / `bash` tool calls that
/// produced file paths, then verify they exist on disk. Each verified path
/// is recorded as an `Artifact`.
async fn collect_file_artifacts(
    messages: &[AgentMessage],
    env: &dyn ExecutionEnv,
    artifacts: &mut Vec<loop_orchestration::workflow::types::Artifact>,
) {
    use loop_ai::Message;
    use loop_orchestration::workflow::types::{Artifact, ArtifactKind};
    use std::path::Path;

    for msg in messages {
        let tr = match msg.as_llm() {
            Some(Message::ToolResult(tr)) => tr,
            _ => continue,
        };
        for content in &tr.content {
            let text = match content {
                loop_ai::ToolResultContent::Text(t) => &t.text,
                _ => continue,
            };
            // Detect "Wrote N bytes to <path>" from the write tool
            if let Some(path_str) = text.strip_prefix("Wrote ") {
                if let Some(idx) = path_str.find(" bytes to ") {
                    let path = path_str[idx + " bytes to ".len()..].trim();
                    if env.file_info(Path::new(path)).await.is_ok() {
                        artifacts.push(Artifact {
                            kind: ArtifactKind::File,
                            path: Some(path.to_string()),
                            data: serde_json::Value::Null,
                        });
                    }
                }
            }
        }
    }
}

/// Create a `spawn_task` tool that allows an agent to dynamically add tasks.
pub fn create_spawn_task_tool(
    workflow_engine: Arc<loop_orchestration::WorkflowEngine>,
    workflow_id: String,
) -> AgentTool {
    AgentTool::simple(
        "spawn_task",
        "Spawn Task",
        "Dynamically spawn a new sub-task in the workflow. The task will be scheduled and executed by an available worker.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "description": {
                    "type": "string",
                    "description": "Description of the task to spawn"
                },
                "prompt": {
                    "type": "string",
                    "description": "Prompt for the agent (if agent_turn type)"
                },
                "kind": {
                    "type": "string",
                    "enum": ["agent_turn", "shell_command"],
                    "description": "Type of task to spawn"
                },
                "command": {
                    "type": "string",
                    "description": "Shell command (if shell_command type)"
                },
                "depends_on": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Task IDs this task depends on"
                }
            },
            "required": ["description", "kind"]
        }),
        move |_id, args, _cancel, _on_update| {
            let engine = Arc::clone(&workflow_engine);
            let wf_id = workflow_id.clone();
            async move {
                let description = args.get("description").and_then(|v| v.as_str()).unwrap_or("spawned task");
                let kind_str = args.get("kind").and_then(|v| v.as_str()).unwrap_or("agent_turn");
                let depends_on: Vec<String> = args
                    .get("depends_on")
                    .and_then(|v| serde_json::from_value(v.clone()).ok())
                    .unwrap_or_default();

                let task_id = format!("dynamic_{}", uuid::Uuid::now_v7());

                let kind = match kind_str {
                    "agent_turn" => {
                        let prompt = args.get("prompt").and_then(|v| v.as_str()).unwrap_or(description);
                        TaskKind::AgentTurn {
                            prompt: prompt.to_string(),
                            tools: None,
                            model: None,
                        }
                    }
                    "shell_command" => {
                        let command = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
                        TaskKind::ShellCommand { command: command.to_string() }
                    }
                    _ => return Err(format!("unsupported task kind: {kind_str}")),
                };

                let task_node = loop_orchestration::planner::task_graph::TaskNode::new(
                    task_id.clone(),
                    kind,
                    description,
                );

                engine
                    .add_task(&wf_id, task_node, depends_on)
                    .await
                    .map_err(|e| e.to_string())?;

                Ok(AgentToolResult::text(format!("Spawned task: {task_id}")))
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use loop_ai::{AssistantMessage, StopReason, TextContent, ThinkingContent, Usage};

    fn asst(text: Option<&str>, thinking: Option<&str>) -> AgentMessage {
        let mut content = Vec::new();
        if let Some(t) = thinking {
            content.push(AssistantContent::Thinking(ThinkingContent {
                thinking: t.into(),
                thinking_signature: None,
                redacted: None,
            }));
        }
        if let Some(t) = text {
            content.push(AssistantContent::Text(TextContent {
                text: t.into(),
                text_signature: None,
            }));
        }
        AgentMessage::assistant(AssistantMessage {
            content,
            api: "test".into(),
            provider: "test".into(),
            model: "test".into(),
            response_model: None,
            response_id: None,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            raw_stop_reason: None,
            timestamp: 0,
        })
    }

    #[test]
    fn extract_joins_all_assistant_text() {
        let messages = vec![asst(Some("part one"), None), asst(Some("part two"), None)];
        let out = extract_output_from_messages(&messages);
        assert_eq!(out.as_str(), Some("part one\n\npart two"));
    }

    #[test]
    fn extract_falls_back_to_thinking() {
        let messages = vec![asst(None, Some("reasoned summary"))];
        let out = extract_output_from_messages(&messages);
        assert_eq!(out.as_str(), Some("reasoned summary"));
    }

    #[test]
    fn extract_prefers_text_over_thinking() {
        let messages = vec![asst(Some("final"), Some("scratch"))];
        let out = extract_output_from_messages(&messages);
        assert_eq!(out.as_str(), Some("final"));
    }

    fn shell_worker(block: bool, seen: Arc<parking_lot::Mutex<Vec<String>>>) -> ShellWorker {
        let env: Arc<dyn ExecutionEnv> =
            Arc::new(crate::harness::HostExecutionEnv::new(std::env::temp_dir()));
        let before: BeforeToolCallFn = Arc::new(move |ctx, _cancel| {
            let seen = Arc::clone(&seen);
            Box::pin(async move {
                seen.lock().push(format!(
                    "{} {}",
                    ctx.tool_call.name,
                    ctx.args["command"].as_str().unwrap_or_default()
                ));
                block.then(|| BeforeToolCallResult {
                    block: true,
                    reason: Some("denied by test".into()),
                })
            })
        });
        ShellWorker::new(env).with_tool_hooks(WorkflowToolHooks {
            before: Some(before),
            after: None,
        })
    }

    #[tokio::test]
    async fn shell_tasks_go_through_the_before_tool_hook_as_bash() {
        let cancel = tokio_util::sync::CancellationToken::new();
        let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let worker = shell_worker(true, Arc::clone(&seen));
        let reason = worker.blocked("t1", "cargo build", &cancel).await;
        assert_eq!(reason.as_deref(), Some("denied by test"));
        assert_eq!(*seen.lock(), ["bash cargo build"]);

        let worker = shell_worker(false, Arc::new(parking_lot::Mutex::new(Vec::new())));
        assert!(worker.blocked("t1", "cargo build", &cancel).await.is_none());
    }

    #[tokio::test]
    async fn shell_tasks_respect_the_bash_blocklist() {
        let cancel = tokio_util::sync::CancellationToken::new();
        let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let worker = shell_worker(false, Arc::clone(&seen));
        let reason = worker.blocked("t1", "rm -rf /", &cancel).await.unwrap();
        assert!(reason.contains("blocked"), "{reason}");
        assert!(seen.lock().is_empty(), "the blocklist runs before hooks");
    }
}
