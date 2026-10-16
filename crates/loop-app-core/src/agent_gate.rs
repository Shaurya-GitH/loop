//! External agents' tool calls and the tool gate.
//!
//! An ACP agent runs its own tools. Loop sees two things:
//!
//! - **permission requests**, which [`GatedHost`] passes through the gate's
//!   `tool_call` hooks before the user is asked. A block picks the agent's
//!   own "reject once" option and an allow its "allow once" option (never an
//!   "always" one); with no decision the user is asked as before.
//! - **tool-call updates**, which [`AgentToolObserver`] turns into
//!   `tool_result` events once a call finishes. These are observe-only: the
//!   agent already ran the tool.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use loop_agent::harness::{HarnessHookEvent, ToolCallInfo, ToolDecision, ToolSource};
use loop_protocol::{
    ContentBlock, HostFileSystem, HostServices, HostTerminals, InteractionOutcome,
    InteractionRequest, PermissionKind, PermissionOption, SessionUpdate, ToolCallStatus,
    ToolCallUpdate, ToolContent, ToolKind,
};

use crate::tool_gate::ToolGate;

/// A host that runs the gate's hooks on an agent's permission requests
/// before handing them to `inner`.
pub struct GatedHost {
    inner: Arc<dyn HostServices>,
    gate: Arc<ToolGate>,
    agent: String,
    cwd: PathBuf,
}

impl GatedHost {
    /// Gate `agent`'s requests (`agent` is its id, e.g. `chatgpt`).
    pub fn new(
        inner: Arc<dyn HostServices>,
        gate: Arc<ToolGate>,
        agent: impl Into<String>,
        cwd: PathBuf,
    ) -> Self {
        Self {
            inner,
            gate,
            agent: agent.into(),
            cwd,
        }
    }

    fn info(&self, call: &ToolCallUpdate) -> ToolCallInfo {
        agent_call_info(
            &self.agent,
            &self.cwd,
            call.id.0.clone(),
            call.name.as_deref().or(call.title.as_deref()),
            call.kind,
            call.raw_input.clone(),
        )
    }
}

#[async_trait]
impl HostServices for GatedHost {
    async fn interact(&self, request: InteractionRequest) -> InteractionOutcome {
        if let InteractionRequest::Permission {
            tool_call: Some(call),
            options,
            ..
        } = &request
        {
            match self.gate.review_agent_call(self.info(call)).await {
                Some(ToolDecision::Block { reason }) => {
                    let reject = pick(options, &[PermissionKind::RejectOnce])
                        .or_else(|| pick(options, &[PermissionKind::RejectAlways]));
                    return match reject {
                        Some(native_id) => InteractionOutcome::Selected {
                            native_id,
                            note: Some(reason),
                        },
                        None => InteractionOutcome::Rejected { reason },
                    };
                }
                Some(ToolDecision::Allow) => {
                    if let Some(native_id) = pick(options, &[PermissionKind::AllowOnce]) {
                        return InteractionOutcome::Selected {
                            native_id,
                            note: None,
                        };
                    }
                }
                None => {}
            }
        }
        self.inner.interact(request).await
    }

    fn file_system(&self) -> Option<&dyn HostFileSystem> {
        self.inner.file_system()
    }

    fn terminals(&self) -> Option<&dyn HostTerminals> {
        self.inner.terminals()
    }
}

fn pick(options: &[PermissionOption], kinds: &[PermissionKind]) -> Option<String> {
    options
        .iter()
        .find(|o| kinds.contains(&o.kind_hint))
        .map(|o| o.native_id.clone())
}

fn agent_call_info(
    agent: &str,
    cwd: &std::path::Path,
    call_id: String,
    tool: Option<&str>,
    kind: Option<ToolKind>,
    raw_input: Option<serde_json::Value>,
) -> ToolCallInfo {
    ToolCallInfo {
        source: ToolSource::Agent {
            id: agent.to_owned(),
        },
        tool: tool.unwrap_or("tool").to_owned(),
        kind: kind.unwrap_or(ToolKind::Other),
        args: raw_input.unwrap_or(serde_json::Value::Null),
        call_id,
        session_id: String::new(),
        cwd: cwd.to_path_buf(),
        mode: None,
    }
}

/// Follows an agent's tool calls on its update stream and reports each one
/// that finishes as a `tool_result` event.
pub struct AgentToolObserver {
    agent: String,
    cwd: PathBuf,
    open: HashMap<String, ToolCallInfo>,
}

impl AgentToolObserver {
    /// An observer for `agent`'s live updates.
    pub fn new(agent: impl Into<String>, cwd: PathBuf) -> Self {
        Self {
            agent: agent.into(),
            cwd,
            open: HashMap::new(),
        }
    }

    /// The event `update` completes, if any.
    pub fn on_update(&mut self, session: &str, update: &SessionUpdate) -> Option<HarnessHookEvent> {
        match update {
            SessionUpdate::ToolCall(call) => {
                let mut info = agent_call_info(
                    &self.agent,
                    &self.cwd,
                    call.id.0.clone(),
                    Some(call.name.as_deref().unwrap_or(&call.title)),
                    Some(call.kind),
                    call.raw_input.clone(),
                );
                info.session_id = session.to_owned();
                if finished(Some(call.status)) {
                    return Some(result_event(
                        info,
                        call.status == ToolCallStatus::Failed,
                        &call.content,
                    ));
                }
                self.open.insert(call.id.0.clone(), info);
                None
            }
            SessionUpdate::ToolCallUpdate(update) => {
                if let Some(info) = self.open.get_mut(&update.id.0) {
                    if let Some(input) = &update.raw_input {
                        info.args = input.clone();
                    }
                    if let Some(kind) = update.kind {
                        info.kind = kind;
                    }
                }
                if !finished(update.status) {
                    return None;
                }
                let info = self.open.remove(&update.id.0)?;
                Some(result_event(
                    info,
                    update.status == Some(ToolCallStatus::Failed),
                    update.content.as_deref().unwrap_or_default(),
                ))
            }
            _ => None,
        }
    }
}

fn finished(status: Option<ToolCallStatus>) -> bool {
    matches!(
        status,
        Some(ToolCallStatus::Completed | ToolCallStatus::Failed)
    )
}

fn result_event(call: ToolCallInfo, is_error: bool, content: &[ToolContent]) -> HarnessHookEvent {
    let output = content
        .iter()
        .filter_map(|c| match c {
            ToolContent::Content {
                content: ContentBlock::Text { text },
            } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    HarnessHookEvent::ToolResult {
        call,
        is_error,
        output,
    }
}

/// Feed `backend`'s live tool calls to the gate's `tool_result` hooks until
/// the backend's stream closes.
pub fn observe_agent_tools(
    backend: &dyn loop_protocol::AgentBackend,
    gate: Arc<ToolGate>,
    agent: String,
    cwd: PathBuf,
) {
    let mut updates = backend.subscribe();
    tokio::spawn(async move {
        let mut observer = AgentToolObserver::new(agent, cwd);
        loop {
            match updates.recv().await {
                Ok(envelope) => {
                    if envelope.origin != loop_protocol::Origin::Live {
                        continue;
                    }
                    if let Some(event) = observer.on_update(&envelope.session.0, &envelope.update) {
                        gate.observe(event).await;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;
    use loop_protocol::{InteractionId, ToolCall, ToolCallId};
    use serde_json::json;

    /// Records whether the user was asked; answers with the first option.
    struct Asked(AtomicBool);

    #[async_trait]
    impl HostServices for Asked {
        async fn interact(&self, _request: InteractionRequest) -> InteractionOutcome {
            self.0.store(true, Ordering::SeqCst);
            InteractionOutcome::Cancelled
        }
    }

    fn gate_with(hook: serde_json::Value) -> (tempfile::TempDir, Arc<ToolGate>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("h.json");
        std::fs::write(&path, hook.to_string()).unwrap();
        let registry = loop_agent::harness::HookRegistry::default();
        crate::hooks_load::register_hooks(&registry, crate::hooks_load::load_hooks(&[path]));
        let gate = ToolGate::new(Default::default());
        gate.set_hooks(registry);
        (dir, gate)
    }

    fn option(id: &str, kind: PermissionKind) -> PermissionOption {
        PermissionOption {
            native_id: id.into(),
            label: id.into(),
            kind_hint: kind,
        }
    }

    fn exec_request(command: &str) -> InteractionRequest {
        let mut call = ToolCallUpdate::new(ToolCallId("t1".into()));
        call.title = Some(format!("Run {command}"));
        call.kind = Some(ToolKind::Execute);
        call.raw_input = Some(json!({ "command": command }));
        InteractionRequest::Permission {
            id: InteractionId("i1".into()),
            tool_call: Some(call),
            options: vec![
                option("always", PermissionKind::AllowAlways),
                option("once", PermissionKind::AllowOnce),
                option("no", PermissionKind::RejectOnce),
            ],
        }
    }

    async fn ask(gate: Arc<ToolGate>, request: InteractionRequest) -> (InteractionOutcome, bool) {
        let asked = Arc::new(Asked(AtomicBool::new(false)));
        let host = GatedHost::new(asked.clone(), gate, "chatgpt", PathBuf::from("/tmp"));
        let outcome = host.interact(request).await;
        (outcome, asked.0.load(Ordering::SeqCst))
    }

    #[tokio::test]
    async fn a_hook_block_rejects_once_without_asking() {
        let (_dir, gate) = gate_with(json!({
            "name": "no-rm", "on": "tool_call",
            "match": {"source": "agent:chatgpt", "kind": "execute", "args": {"command": "rm -rf*"}},
            "action": {"block": true}
        }));
        let (outcome, asked) = ask(Arc::clone(&gate), exec_request("rm -rf build")).await;
        assert!(!asked);
        assert_eq!(
            outcome,
            InteractionOutcome::Selected {
                native_id: "no".into(),
                note: Some("Blocked by hook no-rm".into())
            }
        );
        let (_, asked) = ask(gate, exec_request("ls")).await;
        assert!(asked, "unmatched requests still reach the user");
    }

    #[tokio::test]
    async fn a_hook_allow_picks_allow_once_never_always() {
        let (_dir, gate) = gate_with(json!({
            "on": "tool_call", "match": {"source": "agent"}, "action": {"allow": true}
        }));
        let (outcome, asked) = ask(gate, exec_request("ls")).await;
        assert!(!asked);
        assert_eq!(
            outcome,
            InteractionOutcome::Selected {
                native_id: "once".into(),
                note: None
            }
        );
    }

    #[tokio::test]
    async fn a_block_without_a_reject_option_is_rejected() {
        let (_dir, gate) = gate_with(json!({"on": "tool_call", "action": {"block": true}}));
        let mut request = exec_request("ls");
        if let InteractionRequest::Permission { options, .. } = &mut request {
            options.retain(|o| o.kind_hint != PermissionKind::RejectOnce);
        }
        let (outcome, asked) = ask(gate, request).await;
        assert!(!asked);
        assert!(matches!(outcome, InteractionOutcome::Rejected { .. }));
    }

    #[test]
    fn finished_agent_calls_become_tool_results() {
        let mut observer = AgentToolObserver::new("chatgpt", PathBuf::from("/tmp"));
        let start = SessionUpdate::ToolCall(ToolCall {
            id: ToolCallId("t1".into()),
            name: None,
            title: "Run cargo test".into(),
            kind: ToolKind::Execute,
            status: ToolCallStatus::InProgress,
            content: Vec::new(),
            locations: Vec::new(),
            raw_input: Some(json!({"command": "cargo test"})),
        });
        assert!(observer.on_update("s1", &start).is_none());

        let mut done = ToolCallUpdate::new(ToolCallId("t1".into()));
        done.status = Some(ToolCallStatus::Failed);
        done.content = Some(vec![ToolContent::Content {
            content: ContentBlock::Text {
                text: "1 failed".into(),
            },
        }]);
        let event = observer
            .on_update("s1", &SessionUpdate::ToolCallUpdate(done))
            .expect("a result");
        let HarnessHookEvent::ToolResult {
            call,
            is_error,
            output,
        } = event
        else {
            panic!("expected a tool result");
        };
        assert!(is_error);
        assert_eq!(output, "1 failed");
        assert_eq!(call.tool, "Run cargo test");
        assert_eq!(call.session_id, "s1");
        assert_eq!(call.args["command"], "cargo test");
        assert_eq!(
            call.source,
            ToolSource::Agent {
                id: "chatgpt".into()
            }
        );
    }
}
