//! The one check every tool call passes, in every mode.
//!
//! Bootstrap installs the gate into the harness's before/after tool slots, so
//! `--print`, the TUI, the desktop app and workflow tasks all go through it.
//! Today it enforces `toolPermissions` deny rules everywhere, and hands the
//! rest to the interactive approver when a surface attached one.

use std::collections::BTreeMap;
use std::sync::Arc;

use loop_agent::harness::AgentHarness;
use loop_agent::{AfterToolCallFn, BeforeToolCallFn, BeforeToolCallResult};
use parking_lot::Mutex;

use crate::tool_approval::{ToolApprovalBridge, ToolPermission};

/// Deny rules plus an optional interactive approver.
#[derive(Default)]
pub struct ToolGate {
    permissions: Mutex<BTreeMap<String, ToolPermission>>,
    approver: Mutex<Option<Arc<ToolApprovalBridge>>>,
}

impl ToolGate {
    /// A gate enforcing `permissions` (from `settings.toolPermissions`).
    pub fn new(permissions: BTreeMap<String, ToolPermission>) -> Arc<Self> {
        Arc::new(Self {
            permissions: Mutex::new(permissions),
            approver: Mutex::new(None),
        })
    }

    /// Install the gate into `harness`'s tool slots.
    pub fn install(self: &Arc<Self>, harness: &AgentHarness) {
        harness.set_before_tool_call(Some(self.before_hook()));
        harness.set_after_tool_call(Some(self.after_hook()));
    }

    /// Ask the user through `approver` (TUI, desktop); `None` runs headless.
    pub fn set_approver(&self, approver: Option<Arc<ToolApprovalBridge>>) {
        if let Some(approver) = &approver {
            approver.set_permissions(self.permissions.lock().clone());
        }
        *self.approver.lock() = approver;
    }

    /// Replace the per-tool permissions (settings changed).
    pub fn set_permissions(&self, permissions: BTreeMap<String, ToolPermission>) {
        if let Some(approver) = self.approver() {
            approver.set_permissions(permissions.clone());
        }
        *self.permissions.lock() = permissions;
    }

    fn approver(&self) -> Option<Arc<ToolApprovalBridge>> {
        self.approver.lock().clone()
    }

    /// Why `tool` may not run at all, or `None`.
    pub fn denied(&self, tool: &str) -> Option<String> {
        (self.permissions.lock().get(tool) == Some(&ToolPermission::Deny))
            .then(|| format!("Tool `{tool}` is denied by settings.toolPermissions"))
    }

    fn before_hook(self: &Arc<Self>) -> BeforeToolCallFn {
        let gate = Arc::clone(self);
        Arc::new(move |ctx, cancel| {
            let gate = Arc::clone(&gate);
            Box::pin(async move {
                if let Some(reason) = gate.denied(&ctx.tool_call.name) {
                    return Some(BeforeToolCallResult {
                        block: true,
                        reason: Some(reason),
                    });
                }
                match gate.approver() {
                    Some(approver) => approver.review_before_tool(ctx, cancel).await,
                    None => None,
                }
            })
        })
    }

    fn after_hook(self: &Arc<Self>) -> AfterToolCallFn {
        let gate = Arc::clone(self);
        Arc::new(move |ctx, cancel| {
            let gate = Arc::clone(&gate);
            Box::pin(async move {
                match gate.approver() {
                    Some(approver) => approver.review_after_file_tool(ctx, cancel).await,
                    None => None,
                }
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use loop_agent::BeforeToolCallContext;
    use loop_ai::{AssistantMessage, StopReason, ToolCall};

    fn call(name: &str) -> BeforeToolCallContext {
        let tool_call = ToolCall {
            id: "1".into(),
            name: name.into(),
            arguments: serde_json::json!({ "command": "ls" }),
            thought_signature: None,
        };
        BeforeToolCallContext {
            assistant_message: AssistantMessage {
                content: Vec::new(),
                api: String::new(),
                provider: String::new(),
                model: String::new(),
                response_model: None,
                response_id: None,
                usage: Default::default(),
                stop_reason: StopReason::ToolUse,
                error_message: None,
                raw_stop_reason: None,
                timestamp: 0,
            },
            args: tool_call.arguments.clone(),
            tool_call,
            context: Default::default(),
        }
    }

    #[tokio::test]
    async fn headless_gate_blocks_denied_tools_only() {
        let gate = ToolGate::new(crate::tool_approval::permissions_from_settings(
            &BTreeMap::from([
                ("bash".into(), "deny".into()),
                ("write".into(), "ask".into()),
            ]),
        ));
        let before = gate.before_hook();
        let blocked = before(call("bash"), None).await.expect("bash is denied");
        assert!(blocked.block);
        assert!(blocked.reason.unwrap().contains("denied"));
        assert!(
            before(call("write"), None).await.is_none(),
            "ask runs when nobody can be asked (until permission modes land)"
        );
        assert!(before(call("read"), None).await.is_none());
    }

    #[tokio::test]
    async fn permission_changes_apply_to_the_next_call() {
        let gate = ToolGate::new(BTreeMap::new());
        let before = gate.before_hook();
        assert!(before(call("bash"), None).await.is_none());
        gate.set_permissions(BTreeMap::from([("bash".into(), ToolPermission::Deny)]));
        assert!(before(call("bash"), None).await.is_some());
    }
}
