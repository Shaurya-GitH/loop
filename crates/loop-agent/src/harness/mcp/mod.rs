//! MCP adapter: bridges `loop-mcp` with `loop-agent` tool types.

pub mod bridge;

pub use bridge::mcp_tools_to_agent_tools;

use std::sync::RwLock;

use async_trait::async_trait;
use loop_mcp::server::{ToolDef, ToolOutput, ToolOutputContent, ToolProvider};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::types::{
    AfterToolCallContext, AfterToolCallFn, AgentTool, AgentToolResult, BeforeToolCallContext,
    BeforeToolCallFn,
};

/// Adapter that wraps a `Vec<AgentTool>` as a `loop_mcp::ToolProvider`.
pub struct LoopToolProvider {
    tools: RwLock<Vec<AgentTool>>,
    before: Option<BeforeToolCallFn>,
    after: Option<AfterToolCallFn>,
}

impl LoopToolProvider {
    /// Create a provider from a snapshot of agent tools.
    pub fn new(tools: Vec<AgentTool>) -> Self {
        Self {
            tools: RwLock::new(tools),
            before: None,
            after: None,
        }
    }

    /// Run the host's tool checks around every call, as the agent loop does.
    pub fn with_tool_hooks(
        mut self,
        before: Option<BeforeToolCallFn>,
        after: Option<AfterToolCallFn>,
    ) -> Self {
        self.before = before;
        self.after = after;
        self
    }

    /// Update the tool set at runtime.
    pub async fn set_tools(&self, tools: Vec<AgentTool>) {
        *self.tools.write().expect("tool lock poisoned") = tools;
    }
}

#[async_trait]
impl ToolProvider for LoopToolProvider {
    fn list_tools(&self) -> Vec<ToolDef> {
        let tools = self.tools.read().expect("tool lock poisoned");
        tools
            .iter()
            .map(|t| ToolDef {
                name: t.name.clone(),
                description: t.description.clone(),
                parameters: t.parameters.clone(),
            })
            .collect()
    }

    async fn call_tool(&self, name: &str, args: Value) -> Result<ToolOutput, String> {
        let tool = {
            let tools = self.tools.read().expect("tool lock poisoned");
            tools
                .iter()
                .find(|t| t.name == name)
                .cloned()
                .ok_or_else(|| format!("tool not found: {name}"))?
        };
        let args = match &tool.prepare_arguments {
            Some(prepare) => prepare(args),
            None => args,
        };
        let args = loop_ai::validate_tool_arguments(&tool.to_llm_tool(), &args)
            .map_err(|e| e.to_string())?;

        let tool_call = loop_ai::ToolCall {
            id: loop_ai::new_id(),
            name: name.to_owned(),
            arguments: args.clone(),
            thought_signature: None,
        };
        let cancel = CancellationToken::new();
        let before_ctx = BeforeToolCallContext::standalone(tool_call.clone(), args.clone());
        let assistant_message = before_ctx.assistant_message.clone();
        if let Some(before) = &self.before {
            if let Some(decision) = before(before_ctx, Some(cancel.clone())).await {
                if decision.block {
                    return Err(decision
                        .reason
                        .unwrap_or_else(|| "Tool execution was blocked".into()));
                }
            }
        }

        let (mut result, mut is_error) = match (tool.execute)(
            tool_call.id.clone(),
            args.clone(),
            Some(cancel.clone()),
            None,
        )
        .await
        {
            Ok(result) => (result, false),
            Err(e) => (AgentToolResult::text(e), true),
        };
        if let Some(after) = &self.after {
            let ctx = AfterToolCallContext {
                assistant_message,
                tool_call,
                args,
                result: result.clone(),
                is_error,
                context: Default::default(),
            };
            if let Some(patch) = after(ctx, Some(cancel)).await {
                if let Some(content) = patch.content {
                    result.content = content;
                }
                if let Some(flag) = patch.is_error {
                    is_error = flag;
                }
            }
        }
        if is_error {
            return Err(text_of(&result));
        }

        let content = result
            .content
            .into_iter()
            .map(|c| match c {
                loop_ai::ToolResultContent::Text(t) => ToolOutputContent::Text(t.text),
                loop_ai::ToolResultContent::Image(img) => ToolOutputContent::Image {
                    data: img.data,
                    mime_type: img.mime_type,
                },
            })
            .collect();

        Ok(ToolOutput {
            content,
            is_error: false,
        })
    }
}

fn text_of(result: &AgentToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| match c {
            loop_ai::ToolResultContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    use super::*;
    use crate::types::BeforeToolCallResult;

    fn echo(ran: Arc<AtomicBool>) -> AgentTool {
        AgentTool::simple(
            "echo",
            "echo",
            "echo text",
            serde_json::json!({
                "type": "object",
                "properties": {"text": {"type": "string"}},
                "required": ["text"]
            }),
            move |_, args, _, _| {
                let ran = Arc::clone(&ran);
                async move {
                    ran.store(true, Ordering::SeqCst);
                    Ok(AgentToolResult::text(
                        args["text"].as_str().unwrap_or_default(),
                    ))
                }
            },
        )
    }

    fn blocking(block: bool) -> BeforeToolCallFn {
        Arc::new(move |ctx, _| {
            Box::pin(async move {
                assert_eq!(ctx.tool_call.name, "echo");
                block.then(|| BeforeToolCallResult {
                    block: true,
                    reason: Some("blocked by policy".into()),
                })
            })
        })
    }

    #[tokio::test]
    async fn the_before_hook_can_block_a_client_call() {
        let ran = Arc::new(AtomicBool::new(false));
        let provider = LoopToolProvider::new(vec![echo(Arc::clone(&ran))])
            .with_tool_hooks(Some(blocking(true)), None);
        let Err(err) = provider
            .call_tool("echo", serde_json::json!({"text": "hi"}))
            .await
        else {
            panic!("the call should be blocked");
        };
        assert_eq!(err, "blocked by policy");
        assert!(!ran.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn allowed_calls_run_and_invalid_arguments_are_refused() {
        let ran = Arc::new(AtomicBool::new(false));
        let provider = LoopToolProvider::new(vec![echo(Arc::clone(&ran))])
            .with_tool_hooks(Some(blocking(false)), None);
        let out = provider
            .call_tool("echo", serde_json::json!({"text": "hi"}))
            .await
            .unwrap();
        assert!(matches!(&out.content[0], ToolOutputContent::Text(t) if t == "hi"));
        assert!(ran.load(Ordering::SeqCst));

        ran.store(false, Ordering::SeqCst);
        assert!(provider
            .call_tool("echo", serde_json::json!({}))
            .await
            .is_err());
        assert!(!ran.load(Ordering::SeqCst), "invalid arguments never run");
    }
}
