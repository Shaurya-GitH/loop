//! The one check every tool call passes, in every mode.
//!
//! Bootstrap installs the gate into the harness's before/after tool slots, so
//! `--print`, the TUI, the desktop app and workflow tasks all go through it.
//! Each call passes, in order: `toolPermissions` deny rules, then hooks
//! (`tool_call`: allow or block), then the interactive approver when a
//! surface attached one. After the call, the approver's file review runs,
//! then `tool_result` hooks, which may add context for the model.

use std::collections::BTreeMap;
use std::sync::{Arc, Weak};

use loop_agent::harness::{
    AgentHarness, HarnessHookEvent, HookRegistry, ToolCallInfo, ToolDecision, ToolSource,
};
use loop_agent::{AfterToolCallFn, BeforeToolCallFn, BeforeToolCallResult, ToolCall};
use loop_ai::{TextContent, ToolResultContent};
use parking_lot::Mutex;

use crate::tool_approval::{ToolApprovalBridge, ToolPermission};

/// Largest tool output passed to `tool_result` hooks.
const HOOK_OUTPUT_CAP: usize = 64 * 1024;

/// Deny rules, hooks and an optional interactive approver.
#[derive(Default)]
pub struct ToolGate {
    permissions: Mutex<BTreeMap<String, ToolPermission>>,
    approver: Mutex<Option<Arc<ToolApprovalBridge>>>,
    hooks: Mutex<Option<HookRegistry>>,
    harness: Mutex<Weak<AgentHarness>>,
}

impl ToolGate {
    /// A gate enforcing `permissions` (from `settings.toolPermissions`).
    pub fn new(permissions: BTreeMap<String, ToolPermission>) -> Arc<Self> {
        Arc::new(Self {
            permissions: Mutex::new(permissions),
            ..Default::default()
        })
    }

    /// Install the gate into `harness`'s tool slots and run its hooks.
    pub fn install(self: &Arc<Self>, harness: &Arc<AgentHarness>) {
        *self.hooks.lock() = Some(harness.hooks());
        *self.harness.lock() = Arc::downgrade(harness);
        harness.set_before_tool_call(Some(self.before_hook()));
        harness.set_after_tool_call(Some(self.after_hook()));
    }

    #[cfg(test)]
    fn set_hooks(&self, hooks: HookRegistry) {
        *self.hooks.lock() = Some(hooks);
    }

    /// The hook registry, when it has handlers.
    fn hooks(&self) -> Option<HookRegistry> {
        self.hooks.lock().clone().filter(|hooks| !hooks.is_empty())
    }

    /// What hooks are told about `tool_call`.
    async fn call_info(&self, tool_call: &ToolCall, args: &serde_json::Value) -> ToolCallInfo {
        let harness = self.harness.lock().upgrade();
        let (session_id, cwd) = match harness {
            Some(harness) => (harness.session_id().await, harness.cwd()),
            None => (String::new(), std::path::PathBuf::new()),
        };
        ToolCallInfo {
            source: ToolSource::of_native_tool(&tool_call.name),
            tool: tool_call.name.clone(),
            kind: loop_agent::protocol_events::tool_kind(&tool_call.name),
            args: args.clone(),
            call_id: tool_call.id.clone(),
            session_id,
            cwd,
        }
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
                if let Some(hooks) = gate.hooks() {
                    let info = gate.call_info(&ctx.tool_call, &ctx.args).await;
                    let emit = hooks.emit(HarnessHookEvent::ToolCall(info));
                    // Cancelling the turn drops the emit, which kills any
                    // command hook still running.
                    let outcome = match &cancel {
                        Some(cancel) => tokio::select! {
                            outcome = emit => outcome,
                            () = cancel.cancelled() => {
                                return Some(BeforeToolCallResult {
                                    block: true,
                                    reason: Some("Operation aborted".into()),
                                });
                            }
                        },
                        None => emit.await,
                    };
                    match outcome.decision {
                        Some(ToolDecision::Block { reason }) => {
                            return Some(BeforeToolCallResult {
                                block: true,
                                reason: Some(reason),
                            });
                        }
                        Some(ToolDecision::Allow) => return None,
                        None => {}
                    }
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
                let patch = match gate.approver() {
                    Some(approver) => approver.review_after_file_tool(ctx.clone(), cancel).await,
                    None => None,
                };
                let Some(hooks) = gate.hooks() else {
                    return patch;
                };
                let mut patch = patch.unwrap_or_default();
                let content = patch
                    .content
                    .clone()
                    .unwrap_or_else(|| ctx.result.content.clone());
                let event = HarnessHookEvent::ToolResult {
                    call: gate.call_info(&ctx.tool_call, &ctx.args).await,
                    is_error: patch.is_error.unwrap_or(ctx.is_error),
                    output: capped_text(&content),
                };
                if let Some(extra) = hooks.emit(event).await.append_context {
                    let mut content = content;
                    content.push(ToolResultContent::Text(TextContent {
                        text: extra,
                        text_signature: None,
                    }));
                    patch.content = Some(content);
                }
                Some(patch)
            })
        })
    }
}

/// The text parts of a tool result, capped at [`HOOK_OUTPUT_CAP`] bytes.
fn capped_text(content: &[ToolResultContent]) -> String {
    let mut text = String::new();
    for part in content {
        if let ToolResultContent::Text(t) = part {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&t.text);
        }
    }
    if text.len() > HOOK_OUTPUT_CAP {
        let mut end = HOOK_OUTPUT_CAP;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
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

    fn hooks(files: &[serde_json::Value]) -> (tempfile::TempDir, HookRegistry) {
        let dir = tempfile::tempdir().unwrap();
        let paths: Vec<_> = files
            .iter()
            .enumerate()
            .map(|(i, body)| {
                let path = dir.path().join(format!("h{i}.json"));
                std::fs::write(&path, body.to_string()).unwrap();
                path
            })
            .collect();
        let registry = HookRegistry::default();
        crate::hooks_load::register_hooks(&registry, crate::hooks_load::load_hooks(&paths));
        (dir, registry)
    }

    /// A gate with `bash: ask` and a prompting approver; returns the prompt
    /// receiver so tests can see whether the user was asked.
    fn gate_with_approver(
        permissions: &[(&str, &str)],
    ) -> (
        Arc<ToolGate>,
        tokio::sync::mpsc::UnboundedReceiver<crate::tool_approval::ApprovalPrompt>,
    ) {
        let permissions = crate::tool_approval::permissions_from_settings(
            &permissions
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        );
        let gate = ToolGate::new(permissions.clone());
        let env: Arc<dyn loop_agent::harness::ExecutionEnv> = Arc::new(
            loop_agent::harness::HostExecutionEnv::new(std::env::temp_dir()),
        );
        let (bridge, prompts) =
            ToolApprovalBridge::new(env, None, true, permissions, Default::default());
        gate.set_approver(Some(Arc::new(bridge)));
        (gate, prompts)
    }

    #[tokio::test]
    async fn hook_allow_skips_the_prompt() {
        let (gate, mut prompts) = gate_with_approver(&[("bash", "ask")]);
        let (_dir, registry) = hooks(&[serde_json::json!(
            {"on": "tool_call", "match": {"tool": "bash"}, "action": {"allow": true}}
        )]);
        gate.set_hooks(registry);
        assert!(gate.before_hook()(call("bash"), None).await.is_none());
        assert!(prompts.try_recv().is_err(), "the user was not asked");
    }

    #[tokio::test]
    async fn deny_rules_beat_hook_allow() {
        let (gate, _prompts) = gate_with_approver(&[("bash", "deny")]);
        let (_dir, registry) = hooks(&[serde_json::json!(
            {"on": "tool_call", "action": {"allow": true}}
        )]);
        gate.set_hooks(registry);
        let result = gate.before_hook()(call("bash"), None).await.unwrap();
        assert!(result.block);
        assert!(result.reason.unwrap().contains("toolPermissions"));
    }

    #[tokio::test]
    async fn hook_block_reaches_the_model_without_asking() {
        let (gate, mut prompts) = gate_with_approver(&[("bash", "ask")]);
        let (_dir, registry) = hooks(&[serde_json::json!(
            {"name": "no-ls", "on": "tool_call", "match": {"args": {"command": "ls*"}},
             "action": {"block": true, "reason": "use read"}}
        )]);
        gate.set_hooks(registry);
        let result = gate.before_hook()(call("bash"), None).await.unwrap();
        assert!(result.block);
        assert_eq!(
            result.reason.as_deref(),
            Some("Blocked by hook no-ls: use read")
        );
        assert!(prompts.try_recv().is_err());
    }

    #[tokio::test]
    async fn tool_result_hooks_append_context() {
        let gate = ToolGate::new(BTreeMap::new());
        let (_dir, registry) = hooks(&[serde_json::json!(
            {"on": "tool_result", "match": {"tool": "bash"},
             "action": {"appendContext": "note from hook"}}
        )]);
        gate.set_hooks(registry);
        let before = call("bash");
        let ctx = loop_agent::AfterToolCallContext {
            assistant_message: before.assistant_message,
            tool_call: before.tool_call,
            args: before.args,
            result: loop_agent::AgentToolResult::text("files"),
            is_error: false,
            context: Default::default(),
        };
        let patch = gate.after_hook()(ctx, None).await.unwrap();
        let texts: Vec<_> = patch
            .content
            .unwrap()
            .into_iter()
            .filter_map(|c| match c {
                ToolResultContent::Text(t) => Some(t.text),
                _ => None,
            })
            .collect();
        assert_eq!(texts, ["files", "note from hook"]);
    }

    #[test]
    fn hook_output_is_capped_on_a_char_boundary() {
        let long = "é".repeat(HOOK_OUTPUT_CAP);
        let text = capped_text(&[ToolResultContent::Text(TextContent {
            text: long,
            text_signature: None,
        })]);
        assert!(text.len() <= HOOK_OUTPUT_CAP);
    }
}
