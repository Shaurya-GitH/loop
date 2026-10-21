//! Typed hook registry for harness lifecycle events.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use loop_protocol::ToolKind;
use parking_lot::Mutex;
use serde::Serialize;

use crate::types::AgentMessage;

/// Where a tool call comes from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolSource {
    /// One of Loop's own tools.
    Native,
    /// A tool from an MCP server (`mcp__{server}__{tool}`).
    Mcp {
        /// Server name.
        server: String,
    },
    /// A tool an external agent runs itself (ACP).
    Agent {
        /// Agent id, e.g. `chatgpt`.
        id: String,
    },
}

impl ToolSource {
    /// The source of one of the harness's tools, from its name.
    pub fn of_native_tool(name: &str) -> Self {
        match name
            .strip_prefix("mcp__")
            .and_then(|rest| rest.split_once("__"))
        {
            Some((server, _)) => Self::Mcp {
                server: server.to_owned(),
            },
            None => Self::Native,
        }
    }
}

/// One tool call, as hooks see it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallInfo {
    /// Where the tool comes from.
    pub source: ToolSource,
    /// Tool name (`bash`, `mcp__github__create_issue`) or an agent's title.
    pub tool: String,
    /// What the tool does.
    pub kind: ToolKind,
    /// Validated arguments (an agent's raw input).
    pub args: serde_json::Value,
    /// Tool call id.
    pub call_id: String,
    /// Session the call belongs to.
    pub session_id: String,
    /// Working directory of the session.
    pub cwd: PathBuf,
}

/// A hook's decision about a tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolDecision {
    /// Run it without asking the user (deny rules still apply).
    Allow,
    /// Do not run it; `reason` goes to the model.
    Block {
        /// Why the call was blocked.
        reason: String,
    },
}

type Handler = Arc<
    dyn Fn(HarnessHookEvent) -> Pin<Box<dyn Future<Output = HookOutcome> + Send>> + Send + Sync,
>;

/// Harness hook events.
#[derive(Debug, Clone)]
pub enum HarnessHookEvent {
    /// Before an agent turn starts.
    BeforeAgentStart {
        /// Messages in the turn snapshot.
        messages: Vec<AgentMessage>,
    },
    /// Before session compaction.
    SessionBeforeCompact {
        /// Cut index from compaction preparation.
        preparation_cut: usize,
        /// Optional custom summarizer instructions.
        custom_instructions: Option<String>,
    },
    /// Before navigating the session tree.
    SessionBeforeTree {
        /// Target entry id.
        target_id: String,
    },
    /// Harness settled to idle after a turn.
    Settled,
    /// Steering / follow-up / next-turn queue changed.
    QueueUpdate,
    /// Shutdown was requested.
    ShutdownRequested,
    /// A tool is about to run; hooks may allow or block it.
    ToolCall(ToolCallInfo),
    /// A tool finished; hooks may add context for the model.
    ToolResult {
        /// The call.
        call: ToolCallInfo,
        /// Whether the result is an error.
        is_error: bool,
        /// Text of the result (capped).
        output: String,
    },
    /// A multi-agent workflow was started.
    #[cfg(feature = "orchestration")]
    WorkflowStarted {
        /// Workflow identifier.
        workflow_id: String,
        /// Number of tasks in the graph.
        task_count: usize,
    },
    /// A task within a workflow started executing.
    #[cfg(feature = "orchestration")]
    WorkflowTaskStarted {
        /// Workflow identifier.
        workflow_id: String,
        /// Task identifier.
        task_id: String,
        /// Task description.
        description: String,
    },
    /// A task within a workflow completed.
    #[cfg(feature = "orchestration")]
    WorkflowTaskCompleted {
        /// Workflow identifier.
        workflow_id: String,
        /// Task identifier.
        task_id: String,
        /// Whether the task succeeded.
        success: bool,
    },
    /// A multi-agent workflow completed.
    #[cfg(feature = "orchestration")]
    WorkflowCompleted {
        /// Workflow identifier.
        workflow_id: String,
        /// Whether the workflow succeeded overall.
        success: bool,
    },
}

/// Outcome from a hook handler (field-merge across handlers).
#[derive(Debug, Clone, Default)]
pub struct HookOutcome {
    /// Cancel the pending operation.
    pub cancel: bool,
    /// Optional summary supplied by hook (compact / tree).
    pub summary: Option<String>,
    /// Decision about a [`HarnessHookEvent::ToolCall`].
    pub decision: Option<ToolDecision>,
    /// Text added to a tool result the model sees
    /// ([`HarnessHookEvent::ToolResult`]).
    pub append_context: Option<String>,
}

impl HookOutcome {
    /// Fold `other` into `self`: any cancel cancels, the first summary wins,
    /// a block beats an allow (reasons are joined), contexts are joined.
    pub fn merge(&mut self, other: HookOutcome) {
        self.cancel |= other.cancel;
        if self.summary.is_none() {
            self.summary = other.summary;
        }
        self.decision = match (self.decision.take(), other.decision) {
            (Some(ToolDecision::Block { reason: a }), Some(ToolDecision::Block { reason: b })) => {
                Some(ToolDecision::Block {
                    reason: format!("{a}; {b}"),
                })
            }
            (Some(block @ ToolDecision::Block { .. }), _)
            | (_, Some(block @ ToolDecision::Block { .. })) => Some(block),
            (Some(ToolDecision::Allow), _) | (_, Some(ToolDecision::Allow)) => {
                Some(ToolDecision::Allow)
            }
            (None, None) => None,
        };
        self.append_context = match (self.append_context.take(), other.append_context) {
            (Some(a), Some(b)) => Some(format!("{a}\n{b}")),
            (a, b) => a.or(b),
        };
    }
}

/// Registry of harness hook handlers.
#[derive(Clone, Default)]
pub struct HookRegistry {
    handlers: Arc<Mutex<Vec<Handler>>>,
}

impl HookRegistry {
    /// Register a hook handler.
    pub fn on<F, Fut>(&self, handler: F)
    where
        F: Fn(HarnessHookEvent) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HookOutcome> + Send + 'static,
    {
        self.handlers
            .lock()
            .push(Arc::new(move |event| Box::pin(handler(event))));
    }

    /// Emit an event to all handlers and merge their outcomes
    /// ([`HookOutcome::merge`]).
    pub async fn emit(&self, event: HarnessHookEvent) -> HookOutcome {
        let handlers = self.handlers.lock().clone();
        let mut merged = HookOutcome::default();
        for handler in handlers {
            merged.merge(handler(event.clone()).await);
        }
        merged
    }

    /// Whether any handler is registered.
    pub fn is_empty(&self) -> bool {
        self.handlers.lock().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(reason: &str) -> HookOutcome {
        HookOutcome {
            decision: Some(ToolDecision::Block {
                reason: reason.into(),
            }),
            ..Default::default()
        }
    }

    fn allow() -> HookOutcome {
        HookOutcome {
            decision: Some(ToolDecision::Allow),
            ..Default::default()
        }
    }

    #[test]
    fn block_beats_allow_in_either_order() {
        for (a, b) in [(allow(), block("no")), (block("no"), allow())] {
            let mut merged = a;
            merged.merge(b);
            assert_eq!(
                merged.decision,
                Some(ToolDecision::Block {
                    reason: "no".into()
                })
            );
        }
    }

    #[test]
    fn block_reasons_and_contexts_are_joined() {
        let mut merged = block("a");
        merged.append_context = Some("x".into());
        let mut other = block("b");
        other.append_context = Some("y".into());
        merged.merge(other);
        assert_eq!(
            merged.decision,
            Some(ToolDecision::Block {
                reason: "a; b".into()
            })
        );
        assert_eq!(merged.append_context.as_deref(), Some("x\ny"));
    }

    #[test]
    fn mcp_tools_are_recognised_by_name() {
        assert_eq!(ToolSource::of_native_tool("bash"), ToolSource::Native);
        assert_eq!(
            ToolSource::of_native_tool("mcp__github__create_issue"),
            ToolSource::Mcp {
                server: "github".into()
            }
        );
    }
}
