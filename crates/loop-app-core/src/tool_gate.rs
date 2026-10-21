//! The one check every tool call passes, in every mode.
//!
//! Bootstrap installs the gate into the harness's before/after tool slots, so
//! `--print`, the TUI, the desktop app and workflow tasks all go through it.
//! Each call passes, in order:
//!
//! 1. `toolPermissions` deny rules and the `read-only` ceiling: refused;
//! 2. hooks (`tool_call`): a block refuses, an allow runs it without asking;
//! 3. the permission mode ([`crate::permissions`]): allow, or ask;
//! 4. "ask": the interactive approver when a surface attached one; with
//!    nobody to ask (`--print`, `mcp serve`) the call is refused.
//!
//! After the call, the approver's file review runs (unless step 2 or 3
//! allowed the call), then `tool_result` hooks, which may add context for
//! the model.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Weak};

use loop_agent::harness::{
    AgentHarness, HarnessHookEvent, HookRegistry, ToolCallInfo, ToolDecision, ToolSource,
};
use loop_agent::{AfterToolCallFn, BeforeToolCallFn, BeforeToolCallResult, ToolCall};
use loop_ai::{TextContent, ToolResultContent};
use parking_lot::Mutex;

use loop_protocol::ToolKind;

use crate::permissions::{Decision, PermissionMode};
use crate::tool_approval::{ToolApprovalBridge, ToolPermission};

/// Why a call that needs approval is refused when nobody can approve it,
/// naming a mode that would allow it.
fn headless_refusal(tool: &str, mode: PermissionMode) -> String {
    let fix = if matches!(tool, "write" | "edit") && mode != PermissionMode::AcceptEdits {
        "Rerun with --permission-mode accept-edits (edits inside the workspace) or full-access"
            .to_owned()
    } else if tool == "bash" {
        "Rerun with --permission-mode full-access, or allow the command in \
         settings.toolPermissions, for example \"bash(cargo test*)\": \"allow\""
            .to_owned()
    } else {
        format!(
            "Rerun with --permission-mode full-access, or allow `{tool}` in \
             settings.toolPermissions"
        )
    };
    format!(
        "`{tool}` needs approval in {} mode, and nobody can approve it in this run. {fix}.",
        mode.as_str()
    )
}

/// Largest tool output passed to `tool_result` hooks.
const HOOK_OUTPUT_CAP: usize = 64 * 1024;

/// Deny rules, hooks and an optional interactive approver.
#[derive(Default)]
pub struct ToolGate {
    permissions: Mutex<BTreeMap<String, ToolPermission>>,
    approver: Mutex<Option<Arc<ToolApprovalBridge>>>,
    hooks: Mutex<Option<HookRegistry>>,
    harness: Mutex<Weak<AgentHarness>>,
    mode: Mutex<PermissionMode>,
    workspace: Mutex<Option<PathBuf>>,
    /// Calls allowed without asking, counted by [`call_key`]; their
    /// after-call review is skipped. Keyed by more than the id, which a
    /// server may leave empty or reuse across parallel workflow tasks.
    allowed: Mutex<HashMap<String, usize>>,
    /// MCP servers, for the tools they mark read-only.
    mcp: Mutex<Option<Arc<loop_mcp::McpClientManager>>>,
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
        self.workspace
            .lock()
            .get_or_insert_with(|| crate::permissions::workspace_root(&harness.cwd()));
        *self.hooks.lock() = Some(harness.hooks());
        *self.harness.lock() = Arc::downgrade(harness);
        harness.set_before_tool_call(Some(self.before_hook()));
        harness.set_after_tool_call(Some(self.after_hook()));
    }

    /// The active permission mode.
    pub fn mode(&self) -> PermissionMode {
        *self.mode.lock()
    }

    /// Switch the permission mode; applies to the next call.
    pub fn set_mode(&self, mode: PermissionMode) {
        *self.mode.lock() = mode;
        // A call allowed under the old mode is reviewed under the new one.
        self.allowed.lock().clear();
    }

    /// Look up MCP tools' `readOnlyHint` in `mcp`'s servers.
    pub fn set_mcp(&self, mcp: Arc<loop_mcp::McpClientManager>) {
        *self.mcp.lock() = Some(mcp);
    }

    /// The tool's kind; an MCP tool its server marks read-only is a read.
    async fn kind(&self, tool: &str) -> ToolKind {
        let mcp = self.mcp.lock().clone();
        match (
            mcp,
            loop_agent::harness::mcp::bridge::parse_mcp_tool_name(tool),
        ) {
            (Some(mcp), Some((server, name))) if mcp.read_only_hint(server, name).await => {
                ToolKind::Read
            }
            _ => loop_agent::protocol_events::tool_kind(tool),
        }
    }

    /// The directory `accept-edits` may edit (default: the git root of the
    /// harness's cwd).
    pub fn set_workspace(&self, workspace: PathBuf) {
        *self.workspace.lock() = Some(workspace);
    }

    fn workspace(&self) -> PathBuf {
        self.workspace
            .lock()
            .clone()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_default()
    }

    /// What the mode says about a call, before hooks and the approver.
    pub async fn decide(&self, tool: &str, args: &serde_json::Value) -> Decision {
        let workspace = self.workspace();
        let rule = crate::permissions::rule_for(&self.permissions.lock(), tool, args, &workspace);
        crate::permissions::decide(
            self.mode(),
            rule,
            tool,
            self.kind(tool).await,
            args,
            &workspace,
        )
    }

    /// Run `hooks` without a harness (tests, hosts that only gate agents).
    pub fn set_hooks(&self, hooks: HookRegistry) {
        *self.hooks.lock() = Some(hooks);
    }

    /// The `tool_call` hooks' decision about an external agent's call.
    /// Deny rules name Loop's own tools, so they do not apply here.
    pub async fn review_agent_call(&self, info: ToolCallInfo) -> Option<ToolDecision> {
        let hooks = self.hooks()?;
        hooks.emit(HarnessHookEvent::ToolCall(info)).await.decision
    }

    /// Tell hooks about something that already happened; their answer is
    /// ignored (an agent's finished tool call).
    pub async fn observe(&self, event: HarnessHookEvent) {
        if let Some(hooks) = self.hooks() {
            let _ = hooks.emit(event).await;
        }
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
            kind: self.kind(&tool_call.name).await,
            args: args.clone(),
            call_id: tool_call.id.clone(),
            session_id,
            cwd,
            mode: Some(self.mode().as_str().to_owned()),
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
        self.allowed.lock().clear();
    }

    fn approver(&self) -> Option<Arc<ToolApprovalBridge>> {
        self.approver.lock().clone()
    }

    /// Why `tool` may not run at all, by its plain `toolPermissions` rule.
    pub fn denied(&self, tool: &str) -> Option<String> {
        (self.permissions.lock().get(tool) == Some(&ToolPermission::Deny))
            .then(|| format!("Tool `{tool}` is denied by settings.toolPermissions"))
    }

    fn mark_allowed(&self, call: &ToolCall, args: &serde_json::Value) {
        *self.allowed.lock().entry(call_key(call, args)).or_default() += 1;
    }

    /// Whether `call` was allowed without asking; consumes the mark.
    fn take_allowed(&self, call: &ToolCall, args: &serde_json::Value) -> bool {
        let mut allowed = self.allowed.lock();
        let key = call_key(call, args);
        match allowed.get_mut(&key) {
            Some(count) if *count > 1 => {
                *count -= 1;
                true
            }
            Some(_) => {
                allowed.remove(&key);
                true
            }
            None => false,
        }
    }

    /// The gate's before-call check, for callers outside the harness
    /// (`mcp serve`).
    pub fn before_hook(self: &Arc<Self>) -> BeforeToolCallFn {
        let gate = Arc::clone(self);
        Arc::new(move |ctx, cancel| {
            let gate = Arc::clone(&gate);
            Box::pin(async move {
                let block = |reason: String| {
                    Some(BeforeToolCallResult {
                        block: true,
                        reason: Some(reason),
                    })
                };
                let decision = gate.decide(&ctx.tool_call.name, &ctx.args).await;
                if let Decision::Deny(reason) = decision {
                    return block(reason);
                }
                if let Some(hooks) = gate.hooks() {
                    let info = gate.call_info(&ctx.tool_call, &ctx.args).await;
                    let emit = hooks.emit(HarnessHookEvent::ToolCall(info));
                    // Cancelling the turn drops the emit, which kills any
                    // command hook still running.
                    let outcome = match &cancel {
                        Some(cancel) => tokio::select! {
                            outcome = emit => outcome,
                            () = cancel.cancelled() => return block("Operation aborted".into()),
                        },
                        None => emit.await,
                    };
                    match outcome.decision {
                        Some(ToolDecision::Block { reason }) => return block(reason),
                        Some(ToolDecision::Allow) => {
                            gate.mark_allowed(&ctx.tool_call, &ctx.args);
                            return None;
                        }
                        None => {}
                    }
                }
                if decision == Decision::Allow {
                    gate.mark_allowed(&ctx.tool_call, &ctx.args);
                    return None;
                }
                match gate.approver() {
                    // Only some tools have an approval prompt; asking about
                    // the others would silently run them, so refuse instead.
                    Some(_)
                        if !crate::tool_approval::interactive_tools()
                            .contains(&ctx.tool_call.name.as_str()) =>
                    {
                        block(format!(
                            "`{}` needs approval, but Loop has no approval prompt for it yet. \
                             Set it to allow or deny in settings.toolPermissions.",
                            ctx.tool_call.name
                        ))
                    }
                    Some(approver) => approver.review_before_tool(ctx, cancel).await,
                    None => block(headless_refusal(&ctx.tool_call.name, gate.mode())),
                }
            })
        })
    }

    /// The gate's after-call step, for callers outside the harness.
    pub fn after_hook(self: &Arc<Self>) -> AfterToolCallFn {
        let gate = Arc::clone(self);
        Arc::new(move |ctx, cancel| {
            let gate = Arc::clone(&gate);
            Box::pin(async move {
                let allowed = gate.take_allowed(&ctx.tool_call, &ctx.args);
                let patch = match gate.approver() {
                    Some(approver) if !allowed => {
                        approver.review_after_file_tool(ctx.clone(), cancel).await
                    }
                    _ => None,
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

/// Identifies a call between the before and after hooks.
fn call_key(call: &ToolCall, args: &serde_json::Value) -> String {
    format!("{}\u{0}{}\u{0}{args}", call.id, call.name)
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
    async fn headless_gate_refuses_what_needs_approval() {
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
        let refused = before(call("write"), None)
            .await
            .expect("nobody can approve");
        assert!(refused
            .reason
            .unwrap()
            .contains("needs approval in ask mode"));
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
        let (bridge, prompts) = ToolApprovalBridge::new(env, None, permissions, Default::default());
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

    #[tokio::test]
    async fn the_headless_refusal_names_a_mode_that_would_allow_the_call() {
        let gate = ToolGate::new(crate::tool_approval::permissions_from_settings(
            &Default::default(),
        ));
        gate.set_mode(PermissionMode::AcceptEdits);
        let bash = gate.before_hook()(call("bash"), None).await.unwrap();
        let reason = bash.reason.unwrap();
        assert!(reason.contains("full-access"), "{reason}");
        assert!(!reason.contains("accept-edits ("), "{reason}");
        gate.set_mode(PermissionMode::Ask);
        let write = gate.before_hook()(call("write"), None).await.unwrap();
        assert!(write.reason.unwrap().contains("accept-edits"));
    }

    #[tokio::test]
    async fn read_only_is_a_ceiling_even_for_hook_allow() {
        let (gate, _prompts) = gate_with_approver(&[]);
        gate.set_mode(PermissionMode::ReadOnly);
        let (_dir, registry) = hooks(&[serde_json::json!(
            {"on": "tool_call", "action": {"allow": true}}
        )]);
        gate.set_hooks(registry);
        let result = gate.before_hook()(call("write"), None).await.unwrap();
        assert!(result.reason.unwrap().contains("read-only"));
    }

    #[tokio::test]
    async fn full_access_skips_the_prompt_and_the_edit_review() {
        let (gate, mut prompts) = gate_with_approver(&[("bash", "ask"), ("write", "ask")]);
        gate.set_mode(PermissionMode::FullAccess);
        assert!(gate.before_hook()(call("bash"), None).await.is_none());
        assert!(prompts.try_recv().is_err());

        let before = call("write");
        assert!(gate.before_hook()(before.clone(), None).await.is_none());
        assert_eq!(gate.allowed.lock().len(), 2, "bash and the write");
        let ctx = loop_agent::AfterToolCallContext {
            assistant_message: before.assistant_message,
            tool_call: before.tool_call,
            args: before.args,
            result: loop_agent::AgentToolResult::text("wrote"),
            is_error: false,
            context: Default::default(),
        };
        assert!(gate.after_hook()(ctx, None).await.is_none());
        assert!(
            prompts.try_recv().is_err(),
            "no review after an allowed edit"
        );
        assert_eq!(
            gate.allowed.lock().len(),
            1,
            "the write's mark was consumed"
        );
    }

    #[tokio::test]
    async fn hooks_see_the_mode() {
        let gate = ToolGate::new(BTreeMap::new());
        gate.set_mode(PermissionMode::AcceptEdits);
        let (_dir, registry) = hooks(&[serde_json::json!(
            {"on": "tool_call", "match": {"mode": "accept-edits"},
             "action": {"block": true, "reason": "seen"}}
        )]);
        gate.set_hooks(registry);
        let result = gate.before_hook()(call("read"), None).await.unwrap();
        assert!(result.reason.unwrap().contains("seen"));
    }

    #[tokio::test]
    async fn a_shared_call_id_does_not_skip_another_calls_review() {
        let dir = tempfile::tempdir().unwrap();
        let (gate, _prompts) = gate_with_approver(&[("write", "ask")]);
        gate.set_mode(PermissionMode::AcceptEdits);
        gate.set_workspace(dir.path().to_path_buf());
        let with_path = |path: &str| {
            let mut ctx = call("write");
            ctx.tool_call.id = String::new();
            ctx.args = serde_json::json!({ "path": path, "content": "x" });
            ctx.tool_call.arguments = ctx.args.clone();
            ctx
        };
        let inside = with_path("notes.txt");
        let outside = with_path("/etc/elsewhere.txt");
        assert!(gate.before_hook()(inside.clone(), None).await.is_none());
        assert!(gate.take_allowed(&inside.tool_call, &inside.args));
        assert!(gate.before_hook()(inside.clone(), None).await.is_none());
        assert!(
            !gate.take_allowed(&outside.tool_call, &outside.args),
            "the outside write shares the empty id but was not allowed"
        );
        gate.set_mode(PermissionMode::Ask);
        assert!(
            !gate.take_allowed(&inside.tool_call, &inside.args),
            "changing mode forgets earlier allows"
        );
    }

    #[tokio::test]
    async fn ask_for_a_tool_without_a_prompt_is_refused_not_run() {
        let (gate, mut prompts) = gate_with_approver(&[("mcp__github__*", "ask")]);
        let mut ctx = call("mcp__github__create_issue");
        ctx.tool_call.name = "mcp__github__create_issue".into();
        let result = gate.before_hook()(ctx, None).await.expect("refused");
        assert!(result.reason.unwrap().contains("no approval prompt"));
        assert!(prompts.try_recv().is_err());
    }
}
